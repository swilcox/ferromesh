//! The companion radio protocol: how an app talks to a radio running
//! MeshCore's companion firmware, over USB serial or TCP.
//!
//! Every frame travels as a marker byte, the frame's length as a
//! little-endian `u16`, then the frame: `<` from app to radio, `>` from radio
//! to app. A frame's first byte says what it is. Layouts follow the firmware's
//! `examples/companion_radio/MyMesh.cpp`.
//!
//! Only [`send_text`] and [`send_channel_text`] make the radio transmit.

use crate::error::Result;
use crate::reader::Reader;

/// The protocol version this code speaks. Telling the radio (in
/// [`device_query`]) makes it send the v3 message frames, which carry SNR.
pub const APP_VERSION: u8 = 3;

/// The longest message text the firmware sends. A channel message's text
/// also holds the sender's name and `": "`.
pub const MAX_TEXT_LEN: usize = 160;

/// The length of a contact's stored path.
const MAX_PATH_SIZE: usize = 64;

/// A length beyond any real frame (the firmware's limit is under 256), so the
/// reader must be out of step with the stream.
const MAX_FRAME: usize = 1024;

/// Command codes, the first byte of a frame sent to the radio.
pub mod command {
    pub const APP_START: u8 = 1;
    pub const SEND_TXT_MSG: u8 = 2;
    pub const SEND_CHANNEL_TXT_MSG: u8 = 3;
    pub const GET_CONTACTS: u8 = 4;
    pub const GET_DEVICE_TIME: u8 = 5;
    pub const SET_DEVICE_TIME: u8 = 6;
    pub const SEND_SELF_ADVERT: u8 = 7;
    pub const ADD_UPDATE_CONTACT: u8 = 9;
    pub const SYNC_NEXT_MESSAGE: u8 = 10;
    pub const DEVICE_QUERY: u8 = 22;
    pub const GET_CONTACT_BY_KEY: u8 = 30;
    pub const GET_CHANNEL: u8 = 31;
    pub const SET_CHANNEL: u8 = 32;
    pub const SEND_LOGIN: u8 = 26;
    pub const LOGOUT: u8 = 29;
    pub const SET_OTHER_PARAMS: u8 = 38;
    pub const GET_STATS: u8 = 56;
    pub const SET_AUTOADD_CONFIG: u8 = 58;
    pub const GET_AUTOADD_CONFIG: u8 = 59;
}

/// Codes of frames from the radio. Replies answer the latest command; pushes
/// (0x80 and up) arrive at any time, even between a command and its reply.
pub mod code {
    pub const OK: u8 = 0;
    pub const ERR: u8 = 1;
    pub const CONTACTS_START: u8 = 2;
    pub const CONTACT: u8 = 3;
    pub const END_OF_CONTACTS: u8 = 4;
    pub const SELF_INFO: u8 = 5;
    pub const SENT: u8 = 6;
    pub const CONTACT_MSG_RECV: u8 = 7;
    pub const CHANNEL_MSG_RECV: u8 = 8;
    pub const CURR_TIME: u8 = 9;
    pub const NO_MORE_MESSAGES: u8 = 10;
    pub const DEVICE_INFO: u8 = 13;
    pub const CONTACT_MSG_RECV_V3: u8 = 16;
    pub const CHANNEL_MSG_RECV_V3: u8 = 17;
    pub const CHANNEL_INFO: u8 = 18;
    pub const STATS: u8 = 24;
    pub const AUTOADD_CONFIG: u8 = 25;
    pub const CHANNEL_DATA_RECV: u8 = 27;

    pub const PUSH_ADVERT: u8 = 0x80;
    pub const PUSH_LOGIN_SUCCESS: u8 = 0x85;
    pub const PUSH_LOGIN_FAILED: u8 = 0x86;
    pub const PUSH_SEND_CONFIRMED: u8 = 0x82;
    pub const PUSH_MSG_WAITING: u8 = 0x83;
    pub const PUSH_LOG_RX_DATA: u8 = 0x88;
    pub const PUSH_NEW_ADVERT: u8 = 0x8A;
    pub const PUSH_CONTACT_DELETED: u8 = 0x8F;
    pub const PUSH_CONTACTS_FULL: u8 = 0x90;

    pub const fn is_push(code: u8) -> bool {
        code >= 0x80
    }
}

/// Bits of the auto-add policy, which applies once automatic adding of every
/// node is turned off ([`set_manual_add_contacts`]).
pub mod autoadd {
    /// When the contact table is full, replace the contact heard from least
    /// recently, unless it's a favourite.
    pub const OVERWRITE_OLDEST: u8 = 0x01;
    pub const CHAT: u8 = 0x02;
    pub const REPEATER: u8 = 0x04;
    pub const ROOM_SERVER: u8 = 0x08;
    pub const SENSOR: u8 = 0x10;
}

/// Error codes in [`Frame::Err`].
pub mod error {
    pub const UNSUPPORTED: u8 = 1;
    pub const NOT_FOUND: u8 = 2;
    pub const TABLE_FULL: u8 = 3;
    pub const BAD_STATE: u8 = 4;
    pub const FILE_IO: u8 = 5;
    pub const ILLEGAL_ARG: u8 = 6;

    pub const fn describe(code: u8) -> &'static str {
        match code {
            UNSUPPORTED => "unsupported command",
            NOT_FOUND => "not found",
            TABLE_FULL => "table full",
            BAD_STATE => "bad state",
            FILE_IO => "storage error",
            ILLEGAL_ARG => "invalid argument",
            _ => "unknown error",
        }
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

/// Sets the radio's clock, in Unix seconds. The firmware refuses to move it
/// backwards.
pub fn set_device_time(secs: u32) -> Vec<u8> {
    [&[command::SET_DEVICE_TIME][..], &secs.to_le_bytes()].concat()
}

/// Transmits `text` on the channel in `slot`, stamped `timestamp`. The radio
/// sends `<its name>: <text>` and replies [`Frame::Ok`].
pub fn send_channel_text(slot: u8, timestamp: u32, text: &str) -> Vec<u8> {
    [
        &[command::SEND_CHANNEL_TXT_MSG, txt_type::PLAIN, slot][..],
        &timestamp.to_le_bytes(),
        text.as_bytes(),
    ]
    .concat()
}

/// Transmits a direct message to the contact whose key starts with
/// `recipient`. The reply is [`Frame::Sent`], and the recipient's
/// acknowledgement arrives later as [`Frame::SendConfirmed`].
pub fn send_text(recipient: &[u8; 6], attempt: u8, timestamp: u32, text: &str) -> Vec<u8> {
    [
        &[command::SEND_TXT_MSG, txt_type::PLAIN, attempt][..],
        &timestamp.to_le_bytes(),
        recipient,
        text.as_bytes(),
    ]
    .concat()
}

/// The reply is [`Frame::ChannelInfo`]; unused slots have an empty name and
/// a zero secret.
/// Logs in to a node that keeps members: a room server, whose posts then
/// arrive as messages, or a repeater you administer. The reply is a
/// [`Frame::Sent`] carrying how long to wait, and then the node answers over
/// the air with [`Frame::LoggedIn`] or [`Frame::LoginFailed`]. A radio
/// forgets its logins when it restarts.
pub fn send_login(pubkey: &[u8; 32], password: &str) -> Vec<u8> {
    [&[command::SEND_LOGIN][..], pubkey, password.as_bytes()].concat()
}

/// Ends a session opened by [`send_login`].
pub fn logout(pubkey: &[u8; 32]) -> Vec<u8> {
    [&[command::LOGOUT][..], pubkey].concat()
}

/// Advertises the radio, so others can add it as a contact. A flood advert
/// crosses the mesh and costs everyone airtime; a zero-hop one reaches only
/// the radios that hear it directly. The reply is [`Frame::Ok`], or
/// [`Frame::Err`] with [`error::TABLE_FULL`].
pub fn send_self_advert(flood: bool) -> Vec<u8> {
    vec![command::SEND_SELF_ADVERT, u8::from(flood)]
}

pub fn get_channel(slot: u8) -> Vec<u8> {
    vec![command::GET_CHANNEL, slot]
}

pub fn set_channel(slot: u8, name: &str, secret: &[u8; 16]) -> Vec<u8> {
    [&[command::SET_CHANNEL, slot][..], &padded::<32>(name), secret].concat()
}

/// Lists the radio's contacts: [`Frame::ContactsStart`], a [`Frame::Contact`]
/// for each, then [`Frame::EndOfContacts`].
pub fn get_contacts() -> Vec<u8> {
    vec![command::GET_CONTACTS]
}

/// With `manual` on, only the node types in the auto-add policy are added
/// when their adverts are heard; with it off, every node is. Leaves the
/// radio's other settings alone.
pub fn set_manual_add_contacts(manual: bool) -> Vec<u8> {
    vec![command::SET_OTHER_PARAMS, u8::from(manual)]
}

/// The reply is [`Frame::AutoAddConfig`].
pub fn get_autoadd_config() -> Vec<u8> {
    vec![command::GET_AUTOADD_CONFIG]
}

/// `policy` is a combination of [`autoadd`] bits. `max_hops` limits adding to
/// nodes that close: 0 for no limit, 1 for direct neighbours only.
pub fn set_autoadd_config(policy: u8, max_hops: u8) -> Vec<u8> {
    vec![command::SET_AUTOADD_CONFIG, policy, max_hops]
}

/// The reply is [`Frame::Contact`], or [`Frame::Err`] with
/// [`error::NOT_FOUND`].
pub fn get_contact(pubkey: &[u8; 32]) -> Vec<u8> {
    [&[command::GET_CONTACT_BY_KEY][..], pubkey].concat()
}

/// Adds the contact, or updates it if the radio already has its key.
pub fn add_update_contact(contact: &Contact) -> Vec<u8> {
    let mut path = [0u8; MAX_PATH_SIZE];
    let len = contact.out_path.len().min(MAX_PATH_SIZE);
    path[..len].copy_from_slice(&contact.out_path[..len]);
    [
        &[command::ADD_UPDATE_CONTACT][..],
        &contact.pubkey,
        &[contact.kind, contact.flags, contact.out_path_len.unwrap_or(0xFF)],
        &path,
        &padded::<32>(&contact.name),
        &contact.last_advert.to_le_bytes(),
        &contact.lat_e6.to_le_bytes(),
        &contact.lon_e6.to_le_bytes(),
    ]
    .concat()
}

/// `text` as a NUL-terminated field of `N` bytes, cut at a character
/// boundary if it's too long.
fn padded<const N: usize>(text: &str) -> [u8; N] {
    let mut field = [0u8; N];
    let mut end = text.len().min(N - 1);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    field[..end].copy_from_slice(&text.as_bytes()[..end]);
    field
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
    ChannelInfo(ChannelInfo),
    /// How many contacts [`get_contacts`] is about to list.
    ContactsStart(u32),
    Contact(Contact),
    EndOfContacts,
    AutoAddConfig {
        policy: u8,
        max_hops: u8,
    },
    Sent(Sent),
    SendConfirmed(SendConfirmed),
    /// A node accepted our login, over the air.
    LoggedIn(LoggedIn),
    /// It refused: the wrong password, or none set. Carries the first 6
    /// bytes of its key, when the firmware sends them.
    LoginFailed(Option<[u8; 6]>),
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
            code::CHANNEL_INFO => Self::ChannelInfo(ChannelInfo {
                slot: r.u8()?,
                name: text(r.take(32)?),
                secret: *r.array()?,
            }),
            code::CONTACTS_START => Self::ContactsStart(r.u32_le()?),
            code::CONTACT => Self::Contact(Contact::read(&mut r)?),
            code::END_OF_CONTACTS => Self::EndOfContacts,
            code::AUTOADD_CONFIG => Self::AutoAddConfig { policy: r.u8()?, max_hops: r.u8()? },
            code::SENT => Self::Sent(Sent {
                flood: r.u8()? != 0,
                expected_ack: r.u32_le()?,
                timeout_ms: r.u32_le()?,
            }),
            code::PUSH_SEND_CONFIRMED => {
                Self::SendConfirmed(SendConfirmed { ack: r.u32_le()?, round_trip_ms: r.u32_le()? })
            }
            // Older firmware sends the code alone; newer adds permissions,
            // whose key prefix, and more we don't need.
            code::PUSH_LOGIN_SUCCESS => {
                let permissions = r.u8().unwrap_or(0);
                Self::LoggedIn(LoggedIn { permissions, pubkey_prefix: r.array().ok().copied() })
            }
            code::PUSH_LOGIN_FAILED => {
                let _reserved = r.u8();
                Self::LoginFailed(r.array().ok().copied())
            }
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
    /// 0: every node is added as a contact when heard; 1: only the types in
    /// the auto-add policy.
    pub manual_add_contacts: u8,
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
        // Multi-acks, advert location policy, telemetry modes.
        r.take(3)?;
        let manual_add_contacts = r.u8()?;
        let (freq_khz, bandwidth_hz) = (r.u32_le()?, r.u32_le()?);
        let (spreading_factor, coding_rate) = (r.u8()?, r.u8()?);
        Ok(Self {
            advert_type,
            tx_power_dbm,
            max_tx_power_dbm,
            pubkey,
            lat_e6,
            lon_e6,
            manual_add_contacts,
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

/// A channel slot on the radio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    pub slot: u8,
    pub name: String,
    pub secret: [u8; 16],
}

impl ChannelInfo {
    pub fn is_empty(&self) -> bool {
        self.name.is_empty() && self.secret == [0; 16]
    }
}

/// A node in the radio's contact list. The radio needs a node as a contact
/// to decrypt its direct messages or send it one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub pubkey: [u8; 32],
    /// The advert type: 1 chat, 2 repeater, 3 room server, 4 sensor.
    pub kind: u8,
    /// See [`Contact::FAVOURITE`].
    pub flags: u8,
    /// The known route's length byte as the radio stores it, or `None` to
    /// flood. With it, `out_path` is copied back verbatim.
    pub out_path_len: Option<u8>,
    pub out_path: Vec<u8>,
    pub name: String,
    /// The node's clock in its newest advert, in Unix seconds.
    pub last_advert: u32,
    pub lat_e6: i32,
    pub lon_e6: i32,
}

impl Contact {
    /// A favourite is never replaced to make room for a new contact.
    pub const FAVOURITE: u8 = 0x01;

    pub const fn is_favourite(&self) -> bool {
        self.flags & Self::FAVOURITE != 0
    }

    fn read(r: &mut Reader<'_>) -> Result<Self> {
        let pubkey = *r.array()?;
        let (kind, flags) = (r.u8()?, r.u8()?);
        let out_path_len = hops(r.u8()?);
        let out_path = r.take(MAX_PATH_SIZE)?.to_vec();
        Ok(Self {
            pubkey,
            kind,
            flags,
            out_path_len,
            out_path,
            name: text(r.take(32)?),
            last_advert: r.u32_le()?,
            lat_e6: r.i32_le()?,
            lon_e6: r.i32_le()?,
        })
    }
}

/// A node let us in. `permissions` bit 0 marks an administrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoggedIn {
    pub permissions: u8,
    /// The first 6 bytes of the node's key, when the firmware sends them.
    pub pubkey_prefix: Option<[u8; 6]>,
}

impl LoggedIn {
    pub const fn is_admin(&self) -> bool {
        self.permissions & 1 == 1
    }
}

/// The radio accepted a direct message for sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    /// Flooded, because the radio knows no route to the recipient.
    pub flood: bool,
    /// The code the recipient's acknowledgement will carry.
    pub expected_ack: u32,
    /// How long the radio expects the acknowledgement to take.
    pub timeout_ms: u32,
}

/// A recipient acknowledged a direct message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendConfirmed {
    pub ack: u32,
    pub round_trip_ms: u32,
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
    fn sending_commands() {
        assert_eq!(set_device_time(0x0102_0304), [6, 4, 3, 2, 1]);
        assert_eq!(send_channel_text(2, 1, "hi"), [3, 0, 2, 1, 0, 0, 0, b'h', b'i']);
        assert_eq!(
            send_text(&[9; 6], 0, 1, "yo"),
            [2, 0, 0, 1, 0, 0, 0, 9, 9, 9, 9, 9, 9, b'y', b'o']
        );

        assert_eq!(set_manual_add_contacts(true), [38, 1]);
        assert_eq!(set_autoadd_config(autoadd::OVERWRITE_OLDEST | autoadd::CHAT, 0), [58, 3, 0]);
        let set = set_channel(1, "#test", &[7; 16]);
        assert_eq!(
            (set.len(), &set[..7]),
            (2 + 32 + 16, &[32, 1, b'#', b't', b'e', b's', b't'][..])
        );
        assert_eq!(set[2 + 31], 0, "the name stays NUL-terminated");

        let contact = Contact {
            pubkey: [5; 32],
            kind: 1,
            flags: 0,
            out_path_len: None,
            out_path: Vec::new(),
            name: "Ünïcode name that is far too long to fit".into(),
            last_advert: 7,
            lat_e6: -1,
            lon_e6: 2,
        };
        let frame = add_update_contact(&contact);
        assert_eq!(frame.len(), 1 + 32 + 3 + 64 + 32 + 12);
        assert_eq!(frame[35], 0xFF, "no known path: flood");
        // The radio sends the same layout back, plus a last-modified time.
        let mut reply = frame.clone();
        reply[0] = code::CONTACT;
        reply.extend(99u32.to_le_bytes());
        let Frame::Contact(back) = Frame::parse(&reply).unwrap() else { panic!() };
        assert_eq!(back.pubkey, contact.pubkey);
        assert!(contact.name.starts_with(&back.name) && back.name.len() <= 31, "{}", back.name);
        assert_eq!((back.last_advert, back.lat_e6, back.lon_e6), (7, -1, 2));
    }

    #[test]
    fn sending_replies() {
        let mut info = vec![code::CHANNEL_INFO, 3];
        info.extend(padded::<32>("#test"));
        info.extend([7; 16]);
        let Frame::ChannelInfo(slot) = Frame::parse(&info).unwrap() else { panic!() };
        assert_eq!(
            (slot.slot, slot.name.as_str(), slot.secret, slot.is_empty()),
            (3, "#test", [7; 16], false)
        );

        let sent =
            [&[code::SENT, 1][..], &0xAABB_CCDDu32.to_le_bytes(), &5000u32.to_le_bytes()].concat();
        assert_eq!(
            Frame::parse(&sent).unwrap(),
            Frame::Sent(Sent { flood: true, expected_ack: 0xAABB_CCDD, timeout_ms: 5000 })
        );
        let confirmed = [
            &[code::PUSH_SEND_CONFIRMED][..],
            &0xAABB_CCDDu32.to_le_bytes(),
            &1234u32.to_le_bytes(),
        ]
        .concat();
        assert_eq!(
            Frame::parse(&confirmed).unwrap(),
            Frame::SendConfirmed(SendConfirmed { ack: 0xAABB_CCDD, round_trip_ms: 1234 })
        );
        assert_eq!(error::describe(error::TABLE_FULL), "table full");
        assert_eq!(
            Frame::parse(&[code::PUSH_LOGIN_SUCCESS, 1, 1, 2, 3, 4, 5, 6]).unwrap(),
            Frame::LoggedIn(LoggedIn { permissions: 1, pubkey_prefix: Some([1, 2, 3, 4, 5, 6]) })
        );
        let Ok(Frame::LoggedIn(admin)) = Frame::parse(&[code::PUSH_LOGIN_SUCCESS, 1]) else {
            panic!("a bare success is still a success")
        };
        assert!(admin.is_admin() && admin.pubkey_prefix.is_none());
        assert_eq!(
            Frame::parse(&[code::PUSH_LOGIN_FAILED, 0, 1, 2, 3, 4, 5, 6]).unwrap(),
            Frame::LoginFailed(Some([1, 2, 3, 4, 5, 6]))
        );
        assert_eq!(
            Frame::parse(&[code::CONTACTS_START, 3, 1, 0, 0]).unwrap(),
            Frame::ContactsStart(259)
        );
        assert_eq!(
            Frame::parse(&[code::END_OF_CONTACTS, 0, 0, 0, 0]).unwrap(),
            Frame::EndOfContacts
        );
        assert_eq!(
            Frame::parse(&[code::AUTOADD_CONFIG, 3, 0]).unwrap(),
            Frame::AutoAddConfig { policy: 3, max_hops: 0 }
        );
    }

    #[test]
    fn commands() {
        assert_eq!(encode(&device_query()), [b'<', 2, 0, 22, 3]);
        assert_eq!(app_start("fm"), [1, 0, 0, 0, 0, 0, 0, 0, b'f', b'm']);
        assert_eq!(get_stats(StatsKind::Packets), [56, 2]);
        assert_eq!(send_self_advert(false), [7, 0]);
        assert_eq!(send_self_advert(true), [7, 1]);
        assert_eq!(send_login(&[9; 32], "hunter2"), [&[26][..], &[9; 32], b"hunter2"].concat());
        assert_eq!(logout(&[9; 32]), [&[29][..], &[9; 32]].concat());
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
