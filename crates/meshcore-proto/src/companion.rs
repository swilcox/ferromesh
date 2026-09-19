//! The companion radio protocol: how an app talks to a radio running
//! MeshCore's companion firmware, over USB serial or TCP.
//!
//! Every frame travels as a marker byte, the frame's length as a
//! little-endian `u16`, then the frame: `<` from app to radio, `>` from radio
//! to app. A frame's first byte says what it is. Layouts follow the firmware's
//! `examples/companion_radio/MyMesh.cpp`.
//!
//! None of the commands built here makes the radio transmit.

use crate::error::Result;
use crate::reader::Reader;

/// The protocol version this code speaks. Telling the radio (in
/// [`device_query`]) makes it send the v3 message frames, which carry SNR.
pub const APP_VERSION: u8 = 3;

/// A length beyond any real frame (the firmware's limit is under 256), so the
/// reader must be out of step with the stream.
const MAX_FRAME: usize = 1024;

/// Command codes, the first byte of a frame sent to the radio.
pub mod command {
    pub const APP_START: u8 = 1;
    pub const GET_DEVICE_TIME: u8 = 5;
    pub const SYNC_NEXT_MESSAGE: u8 = 10;
    pub const DEVICE_QUERY: u8 = 22;
    pub const GET_STATS: u8 = 56;
}

/// Codes of frames from the radio. Replies answer the latest command; pushes
/// (0x80 and up) arrive at any time, even between a command and its reply.
pub mod code {
    pub const OK: u8 = 0;
    pub const ERR: u8 = 1;
    pub const SELF_INFO: u8 = 5;
    pub const CONTACT_MSG_RECV: u8 = 7;
    pub const CHANNEL_MSG_RECV: u8 = 8;
    pub const CURR_TIME: u8 = 9;
    pub const NO_MORE_MESSAGES: u8 = 10;
    pub const DEVICE_INFO: u8 = 13;
    pub const CONTACT_MSG_RECV_V3: u8 = 16;
    pub const CHANNEL_MSG_RECV_V3: u8 = 17;
    pub const STATS: u8 = 24;
    pub const CHANNEL_DATA_RECV: u8 = 27;

    pub const PUSH_ADVERT: u8 = 0x80;
    pub const PUSH_MSG_WAITING: u8 = 0x83;
    pub const PUSH_LOG_RX_DATA: u8 = 0x88;
    pub const PUSH_NEW_ADVERT: u8 = 0x8A;

    pub const fn is_push(code: u8) -> bool {
        code >= 0x80
    }
}

/// Text types in message frames.
pub mod txt_type {
    pub const PLAIN: u8 = 0;
    pub const CLI_DATA: u8 = 1;
    /// Room server posts: the text follows a 4-byte prefix of the author's key.
    pub const SIGNED_PLAIN: u8 = 2;
}

/// Wraps a frame for sending to the radio.
pub fn encode(frame: &[u8]) -> Vec<u8> {
    let len = u16::try_from(frame.len()).expect("command frames are short");
    let mut out = Vec::with_capacity(3 + frame.len());
    out.push(b'<');
    out.extend(len.to_le_bytes());
    out.extend(frame);
    out
}

/// Says which protocol version the app speaks; the reply is [`DeviceInfo`].
pub fn device_query() -> Vec<u8> {
    vec![command::DEVICE_QUERY, APP_VERSION]
}

/// Starts a session under `app_name`; the reply is [`SelfInfo`].
pub fn app_start(app_name: &str) -> Vec<u8> {
    let mut frame = vec![command::APP_START, 0, 0, 0, 0, 0, 0, 0];
    frame.extend(app_name.as_bytes());
    frame
}

/// Takes the oldest queued message; the reply is a message frame or
/// [`Frame::NoMoreMessages`].
pub fn sync_next_message() -> Vec<u8> {
    vec![command::SYNC_NEXT_MESSAGE]
}

pub fn get_device_time() -> Vec<u8> {
    vec![command::GET_DEVICE_TIME]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsKind {
    Core = 0,
    Radio = 1,
    Packets = 2,
}

pub fn get_stats(kind: StatsKind) -> Vec<u8> {
    vec![command::GET_STATS, kind as u8]
}

/// Splits the radio's byte stream into frames.
#[derive(Debug, Default)]
pub struct Deframer {
    buf: Vec<u8>,
    skipped: usize,
}

impl Deframer {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole frame, if one has arrived. Bytes that aren't part of a
    /// frame, such as noise before a `>` marker, are skipped.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        loop {
            let start = self.buf.iter().position(|&byte| byte == b'>').unwrap_or(self.buf.len());
            self.skip(start);
            let header = self.buf.get(..3)?;
            let len = usize::from(u16::from_le_bytes([header[1], header[2]]));
            if len == 0 || len > MAX_FRAME {
                self.skip(1);
                continue;
            }
            if self.buf.len() < 3 + len {
                return None;
            }
            let frame = self.buf[3..3 + len].to_vec();
            self.buf.drain(..3 + len);
            return Some(frame);
        }
    }

    /// How many bytes were skipped since the last call.
    pub fn take_skipped(&mut self) -> usize {
        std::mem::take(&mut self.skipped)
    }

    fn skip(&mut self, count: usize) {
        self.buf.drain(..count);
        self.skipped += count;
    }
}

/// A frame from the radio.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame<'a> {
    Ok,
    /// The command failed, with the firmware's error code if it sent one.
    Err(Option<u8>),
    SelfInfo(SelfInfo),
    DeviceInfo(DeviceInfo),
    /// The radio's clock, in Unix seconds.
    CurrentTime(u32),
    NoMoreMessages,
    ContactMessage(ContactMessage<'a>),
    ChannelMessage(ChannelMessage<'a>),
    Stats(Stats),
    RxLog(RxLog<'a>),
    MessageWaiting,
    /// Any other frame, by code.
    Other(u8),
}

impl<'a> Frame<'a> {
    pub fn parse(frame: &'a [u8]) -> Result<Self> {
        let mut r = Reader::frame(frame);
        Ok(match r.u8()? {
            code::OK => Self::Ok,
            code::ERR => Self::Err(r.u8().ok()),
            code::SELF_INFO => Self::SelfInfo(SelfInfo::read(&mut r)?),
            code::DEVICE_INFO => Self::DeviceInfo(DeviceInfo::read(&mut r)?),
            code::CURR_TIME => Self::CurrentTime(r.u32_le()?),
            code::NO_MORE_MESSAGES => Self::NoMoreMessages,
            code::CONTACT_MSG_RECV => Self::ContactMessage(ContactMessage::read(&mut r, None)?),
            code::CONTACT_MSG_RECV_V3 => {
                let snr = snr_v3(&mut r)?;
                Self::ContactMessage(ContactMessage::read(&mut r, Some(snr))?)
            }
            code::CHANNEL_MSG_RECV => Self::ChannelMessage(ChannelMessage::read(&mut r, None)?),
            code::CHANNEL_MSG_RECV_V3 => {
                let snr = snr_v3(&mut r)?;
                Self::ChannelMessage(ChannelMessage::read(&mut r, Some(snr))?)
            }
            code::STATS => Self::Stats(Stats::read(&mut r)?),
            code::PUSH_LOG_RX_DATA => {
                Self::RxLog(RxLog { snr: quarter_db(r.u8()?), rssi: r.u8()? as i8, raw: r.rest() })
            }
            code::PUSH_MSG_WAITING => Self::MessageWaiting,
            other => Self::Other(other),
        })
    }
}

/// v3 message frames start with SNR, then two reserved bytes.
fn snr_v3(r: &mut Reader<'_>) -> Result<f64> {
    let snr = quarter_db(r.u8()?);
    r.take(2)?;
    Ok(snr)
}

/// SNR travels as a signed byte in quarter decibels.
fn quarter_db(byte: u8) -> f64 {
    f64::from(byte as i8) / 4.0
}

/// A NUL-padded or NUL-terminated string field.
fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// The radio's identity and settings, in reply to [`app_start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfInfo {
    pub advert_type: u8,
    pub tx_power_dbm: u8,
    pub max_tx_power_dbm: u8,
    pub pubkey: [u8; 32],
    pub lat_e6: i32,
    pub lon_e6: i32,
    pub freq_khz: u32,
    pub bandwidth_hz: u32,
    pub spreading_factor: u8,
    pub coding_rate: u8,
    pub name: String,
}

impl SelfInfo {
    fn read(r: &mut Reader<'_>) -> Result<Self> {
        let (advert_type, tx_power_dbm, max_tx_power_dbm) = (r.u8()?, r.u8()?, r.u8()?);
        let pubkey = *r.array()?;
        let (lat_e6, lon_e6) = (r.i32_le()?, r.i32_le()?);
        // Multi-acks, advert location policy, telemetry modes, manual add.
        r.take(4)?;
        let (freq_khz, bandwidth_hz) = (r.u32_le()?, r.u32_le()?);
        let (spreading_factor, coding_rate) = (r.u8()?, r.u8()?);
        Ok(Self {
            advert_type,
            tx_power_dbm,
            max_tx_power_dbm,
            pubkey,
            lat_e6,
            lon_e6,
            freq_khz,
            bandwidth_hz,
            spreading_factor,
            coding_rate,
            name: text(r.rest()),
        })
    }

    /// `MHz,kHz,SF,CR`, as observer firmware reports it: `910.525,62.5,7,5`.
    pub fn radio(&self) -> String {
        format!(
            "{},{},{},{}",
            f64::from(self.freq_khz) / 1000.0,
            f64::from(self.bandwidth_hz) / 1000.0,
            self.spreading_factor,
            self.coding_rate
        )
    }
}

/// The firmware, in reply to [`device_query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The companion protocol version the firmware speaks.
    pub protocol_version: u8,
    pub max_contacts: u16,
    pub max_channels: u8,
    pub build_date: String,
    pub model: String,
    pub version: String,
}

impl DeviceInfo {
    fn read(r: &mut Reader<'_>) -> Result<Self> {
        let protocol_version = r.u8()?;
        let max_contacts = u16::from(r.u8()?) * 2;
        let max_channels = r.u8()?;
        // The Bluetooth pairing PIN, which we have no use for.
        r.take(4)?;
        Ok(Self {
            protocol_version,
            max_contacts,
            max_channels,
            build_date: text(r.take(12)?),
            model: text(r.take(40)?),
            version: text(r.take(20)?),
        })
    }
}

/// A direct message to the radio, from its queue.
#[derive(Debug, Clone, PartialEq)]
pub struct ContactMessage<'a> {
    /// Only in v3 frames.
    pub snr: Option<f64>,
    /// The first 6 bytes of the sender's public key.
    pub sender_prefix: [u8; 6],
    /// Hops it travelled, or `None` if it came by a direct route.
    pub path_len: Option<u8>,
    pub txt_type: u8,
    /// The sender's clock when it sent the message, in Unix seconds.
    pub sender_timestamp: u32,
    /// For signed posts, the author's 4-byte key prefix.
    pub signer_prefix: Option<[u8; 4]>,
    pub text: &'a [u8],
}

impl<'a> ContactMessage<'a> {
    fn read(r: &mut Reader<'a>, snr: Option<f64>) -> Result<Self> {
        let sender_prefix = *r.array()?;
        let path_len = hops(r.u8()?);
        let txt_type = r.u8()?;
        let sender_timestamp = r.u32_le()?;
        let signer_prefix =
            if txt_type == txt_type::SIGNED_PLAIN { Some(*r.array()?) } else { None };
        Ok(Self {
            snr,
            sender_prefix,
            path_len,
            txt_type,
            sender_timestamp,
            signer_prefix,
            text: r.rest(),
        })
    }
}

/// A channel message the radio decrypted with one of its channel slots.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMessage<'a> {
    pub snr: Option<f64>,
    /// The radio's slot for the channel.
    pub channel_index: u8,
    pub path_len: Option<u8>,
    pub txt_type: u8,
    pub sender_timestamp: u32,
    /// `Sender Name: text`, as on the air.
    pub text: &'a [u8],
}

impl<'a> ChannelMessage<'a> {
    fn read(r: &mut Reader<'a>, snr: Option<f64>) -> Result<Self> {
        Ok(Self {
            snr,
            channel_index: r.u8()?,
            path_len: hops(r.u8()?),
            txt_type: r.u8()?,
            sender_timestamp: r.u32_le()?,
            text: r.rest(),
        })
    }
}

/// 0xFF marks a message that came by a direct route rather than a flood.
fn hops(path_len: u8) -> Option<u8> {
    (path_len != 0xFF).then_some(path_len)
}

/// Counters from [`get_stats`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stats {
    Core {
        battery_mv: u16,
        uptime_secs: u32,
        error_flags: u16,
        queue_len: u8,
    },
    Radio {
        noise_floor: i16,
        last_rssi: i8,
        last_snr: f64,
        tx_air_secs: u32,
        rx_air_secs: u32,
    },
    Packets {
        received: u32,
        sent: u32,
        sent_flood: u32,
        sent_direct: u32,
        received_flood: u32,
        received_direct: u32,
        receive_errors: u32,
    },
}

impl Stats {
    fn read(r: &mut Reader<'_>) -> Result<Self> {
        Ok(match r.u8()? {
            0 => Self::Core {
                battery_mv: r.u16_le()?,
                uptime_secs: r.u32_le()?,
                error_flags: r.u16_le()?,
                queue_len: r.u8()?,
            },
            1 => Self::Radio {
                noise_floor: r.u16_le()? as i16,
                last_rssi: r.u8()? as i8,
                last_snr: quarter_db(r.u8()?),
                tx_air_secs: r.u32_le()?,
                rx_air_secs: r.u32_le()?,
            },
            _ => Self::Packets {
                received: r.u32_le()?,
                sent: r.u32_le()?,
                sent_flood: r.u32_le()?,
                sent_direct: r.u32_le()?,
                received_flood: r.u32_le()?,
                received_direct: r.u32_le()?,
                receive_errors: r.u32_le()?,
            },
        })
    }
}

/// A packet the radio heard, exactly as received, pushed while an app is
/// connected.
#[derive(Debug, Clone, PartialEq)]
pub struct RxLog<'a> {
    pub snr: f64,
    pub rssi: i8,
    pub raw: &'a [u8],
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![b'>'];
        out.extend((bytes.len() as u16).to_le_bytes());
        out.extend(bytes);
        out
    }

    #[test]
    fn commands() {
        assert_eq!(encode(&device_query()), [b'<', 2, 0, 22, 3]);
        assert_eq!(app_start("fm"), [1, 0, 0, 0, 0, 0, 0, 0, b'f', b'm']);
        assert_eq!(get_stats(StatsKind::Packets), [56, 2]);
    }

    #[test]
    fn deframing_across_reads_and_noise() {
        let mut deframer = Deframer::default();
        let stream =
            [b"xx".as_slice(), &frame(&[0x83]), &frame(&[10]), b"\0>\xff\xff", &frame(&[0])]
                .concat();
        let (first, rest) = stream.split_at(4);
        deframer.push(first);
        assert_eq!(deframer.next_frame(), None);
        deframer.push(rest);
        assert_eq!(deframer.next_frame(), Some(vec![0x83]));
        assert_eq!(deframer.next_frame(), Some(vec![10]));
        // A marker with an impossible length is skipped, then a frame follows.
        assert_eq!(deframer.next_frame(), Some(vec![0]));
        assert_eq!(deframer.next_frame(), None);
        assert_eq!(deframer.take_skipped(), 2 + 1 + 1 + 2);
    }

    #[test]
    fn device_info() {
        let mut f = vec![code::DEVICE_INFO, 13, 175, 40, 1, 2, 3, 4];
        f.extend(*b"14-Aug-2026\0");
        f.extend([b"Heltec V4.3 OLED".as_slice(), &[0; 24]].concat());
        f.extend([b"v1.17.1-d929643".as_slice(), &[0; 5]].concat());
        f.extend([0, 0]);
        assert_eq!(f.len(), 82);
        let Frame::DeviceInfo(info) = Frame::parse(&f).unwrap() else { panic!() };
        assert_eq!(
            info,
            DeviceInfo {
                protocol_version: 13,
                max_contacts: 350,
                max_channels: 40,
                build_date: "14-Aug-2026".into(),
                model: "Heltec V4.3 OLED".into(),
                version: "v1.17.1-d929643".into(),
            }
        );
    }

    #[test]
    fn self_info() {
        let mut f = vec![code::SELF_INFO, 1, 10, 22];
        f.extend([0xAB; 32]);
        f.extend(36_100_000i32.to_le_bytes());
        f.extend((-86_800_000i32).to_le_bytes());
        f.extend([0, 0, 0, 0]);
        f.extend(910_525u32.to_le_bytes());
        f.extend(62_500u32.to_le_bytes());
        f.extend([7, 5]);
        f.extend(b"desk");
        let Frame::SelfInfo(info) = Frame::parse(&f).unwrap() else { panic!() };
        assert_eq!((info.name.as_str(), info.tx_power_dbm, info.pubkey[0]), ("desk", 10, 0xAB));
        assert_eq!((info.lat_e6, info.lon_e6), (36_100_000, -86_800_000));
        assert_eq!(info.radio(), "910.525,62.5,7,5");
    }

    #[test]
    fn messages() {
        let mut dm = vec![code::CONTACT_MSG_RECV_V3, (-10i8) as u8, 0, 0, 1, 2, 3, 4, 5, 6, 3, 0];
        dm.extend(1_789_000_000u32.to_le_bytes());
        dm.extend(b"hello");
        let Frame::ContactMessage(message) = Frame::parse(&dm).unwrap() else { panic!() };
        assert_eq!(
            message,
            ContactMessage {
                snr: Some(-2.5),
                sender_prefix: [1, 2, 3, 4, 5, 6],
                path_len: Some(3),
                txt_type: txt_type::PLAIN,
                sender_timestamp: 1_789_000_000,
                signer_prefix: None,
                text: b"hello",
            }
        );

        let mut signed = vec![code::CONTACT_MSG_RECV, 1, 2, 3, 4, 5, 6, 0xFF, 2, 0, 0, 0, 0];
        signed.extend([9, 9, 9, 9]);
        signed.extend(b"post");
        let Frame::ContactMessage(message) = Frame::parse(&signed).unwrap() else { panic!() };
        assert_eq!(
            (message.snr, message.path_len, message.signer_prefix, message.text),
            (None, None, Some([9; 4]), b"post".as_slice())
        );

        let mut channel = vec![code::CHANNEL_MSG_RECV_V3, 48, 0, 0, 0, 2, 0, 1, 0, 0, 0];
        channel.extend(b"Bob: hi");
        let Frame::ChannelMessage(message) = Frame::parse(&channel).unwrap() else { panic!() };
        assert_eq!(
            (message.snr, message.channel_index, message.path_len),
            (Some(12.0), 0, Some(2))
        );
        assert_eq!(message.text, b"Bob: hi");
    }

    #[test]
    fn pushes_and_stats() {
        let rx = [code::PUSH_LOG_RX_DATA, 17, (-77i8) as u8, 0x15, 0x00, 0xAA];
        assert_eq!(
            Frame::parse(&rx).unwrap(),
            Frame::RxLog(RxLog { snr: 4.25, rssi: -77, raw: &[0x15, 0x00, 0xAA] })
        );
        assert_eq!(Frame::parse(&[code::PUSH_MSG_WAITING]).unwrap(), Frame::MessageWaiting);
        assert_eq!(Frame::parse(&[code::PUSH_NEW_ADVERT, 1]).unwrap(), Frame::Other(0x8A));
        assert_eq!(Frame::parse(&[code::ERR, 2]).unwrap(), Frame::Err(Some(2)));

        let mut radio = vec![code::STATS, 1];
        radio.extend((-82i16).to_le_bytes());
        radio.extend([(-28i8) as u8, 50]);
        radio.extend(0u32.to_le_bytes());
        radio.extend(8u32.to_le_bytes());
        assert_eq!(
            Frame::parse(&radio).unwrap(),
            Frame::Stats(Stats::Radio {
                noise_floor: -82,
                last_rssi: -28,
                last_snr: 12.5,
                tx_air_secs: 0,
                rx_air_secs: 8,
            })
        );
        let mut packets = vec![code::STATS, 2];
        for n in [27u32, 0, 0, 0, 27, 0, 1] {
            packets.extend(n.to_le_bytes());
        }
        let Frame::Stats(Stats::Packets { received, receive_errors, .. }) =
            Frame::parse(&packets).unwrap()
        else {
            panic!()
        };
        assert_eq!((received, receive_errors), (27, 1));

        assert!(Frame::parse(&[code::STATS, 0, 1]).is_err());
    }
}
