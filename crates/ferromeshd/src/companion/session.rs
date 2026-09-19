//! One connection to a companion radio: the handshake, then commands and the
//! pushes that arrive around them.
//!
//! Only read-only commands are used here, so the radio never transmits because
//! of us. (It does transmit on its own: it acknowledges direct messages.)

use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use meshcore_proto::companion::{self, Deframer, DeviceInfo, Frame, SelfInfo, StatsKind, code};
use tracing::debug;

/// How long a command may take to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// More queued messages than the radio can hold, so a runaway loop stops.
const MAX_DRAIN: usize = 64;
const APP_NAME: &str = "ferromeshd";

/// Who the radio is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub info: SelfInfo,
    pub device: DeviceInfo,
}

/// A frame worth keeping, stamped when it arrived: a heard packet or a
/// queued message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub at: Timestamp,
    pub frame: Vec<u8>,
}

pub struct Session<L> {
    link: L,
    deframer: Deframer,
    identity: Identity,
    received: Vec<Received>,
    /// The radio says messages are queued.
    waiting: bool,
    timeout: Duration,
}

impl<L: Read + Write> Session<L> {
    /// Tells the radio which protocol version we speak, then asks who it is.
    pub fn start(link: L) -> Result<Self> {
        Self::start_with(link, REPLY_TIMEOUT)
    }

    fn start_with(link: L, timeout: Duration) -> Result<Self> {
        let placeholder = Identity {
            info: SelfInfo {
                advert_type: 0,
                tx_power_dbm: 0,
                max_tx_power_dbm: 0,
                pubkey: [0; 32],
                lat_e6: 0,
                lon_e6: 0,
                freq_khz: 0,
                bandwidth_hz: 0,
                spreading_factor: 0,
                coding_rate: 0,
                name: String::new(),
            },
            device: DeviceInfo {
                protocol_version: 0,
                max_contacts: 0,
                max_channels: 0,
                build_date: String::new(),
                model: String::new(),
                version: String::new(),
            },
        };
        let mut session = Self {
            link,
            deframer: Deframer::default(),
            identity: placeholder,
            received: Vec::new(),
            waiting: false,
            timeout,
        };
        let reply = session.request(&companion::device_query())?;
        let Frame::DeviceInfo(device) = Frame::parse(&reply)? else {
            bail!("unexpected reply {:#04x} to the device query", reply[0]);
        };
        let reply = session.request(&companion::app_start(APP_NAME))?;
        let Frame::SelfInfo(info) = Frame::parse(&reply)? else {
            bail!("unexpected reply {:#04x} to app start", reply[0]);
        };
        session.identity = Identity { info, device };
        // Anything queued before we connected is fetched by `drain_messages`.
        session.waiting = true;
        Ok(session)
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Frames kept since the last call, oldest first.
    pub fn take_received(&mut self) -> Vec<Received> {
        std::mem::take(&mut self.received)
    }

    /// Handles pushes for up to `wait`, then fetches any queued messages.
    pub fn poll(&mut self, wait: Duration) -> Result<()> {
        let deadline = Instant::now() + wait;
        while let Some(frame) = self.read_frame(deadline)? {
            if code::is_push(frame[0]) {
                self.push(frame);
            } else {
                debug!(code = frame[0], "ignoring an unrequested reply");
            }
        }
        if self.waiting {
            self.drain_messages()?;
        }
        Ok(())
    }

    /// Fetches every queued message.
    pub fn drain_messages(&mut self) -> Result<usize> {
        self.waiting = false;
        for fetched in 0..MAX_DRAIN {
            let reply = self.request(&companion::sync_next_message())?;
            match reply[0] {
                code::NO_MORE_MESSAGES => return Ok(fetched),
                code::CONTACT_MSG_RECV
                | code::CONTACT_MSG_RECV_V3
                | code::CHANNEL_MSG_RECV
                | code::CHANNEL_MSG_RECV_V3
                | code::CHANNEL_DATA_RECV => {
                    self.received.push(Received { at: Timestamp::now(), frame: reply });
                }
                other => bail!("unexpected reply {other:#04x} while fetching messages"),
            }
        }
        Ok(MAX_DRAIN)
    }

    /// The core, radio and packet stats frames, in that order.
    pub fn stats(&mut self) -> Result<Vec<Vec<u8>>> {
        [StatsKind::Core, StatsKind::Radio, StatsKind::Packets]
            .into_iter()
            .map(|kind| {
                let reply = self.request(&companion::get_stats(kind))?;
                match Frame::parse(&reply)? {
                    Frame::Stats(_) => Ok(reply),
                    _ => bail!("unexpected reply {:#04x} to a stats request", reply[0]),
                }
            })
            .collect()
    }

    /// The radio's clock, in Unix seconds.
    pub fn device_time(&mut self) -> Result<u32> {
        let reply = self.request(&companion::get_device_time())?;
        match Frame::parse(&reply)? {
            Frame::CurrentTime(secs) => Ok(secs),
            _ => bail!("unexpected reply {:#04x} to a time request", reply[0]),
        }
    }

    /// Sends a command and returns its reply, handling pushes that arrive first.
    fn request(&mut self, command: &[u8]) -> Result<Vec<u8>> {
        self.link.write_all(&companion::encode(command)).context("writing to the radio")?;
        self.link.flush().context("writing to the radio")?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let Some(frame) = self.read_frame(deadline)? else {
                bail!("no reply to command {} within {:?}", command[0], self.timeout);
            };
            if !code::is_push(frame[0]) {
                return Ok(frame);
            }
            self.push(frame);
        }
    }

    fn push(&mut self, frame: Vec<u8>) {
        match frame[0] {
            code::PUSH_LOG_RX_DATA => self.received.push(Received { at: Timestamp::now(), frame }),
            code::PUSH_MSG_WAITING => self.waiting = true,
            other => debug!(code = other, "ignoring a push"),
        }
    }

    /// The next frame, or `None` once `deadline` passes.
    fn read_frame(&mut self, deadline: Instant) -> Result<Option<Vec<u8>>> {
        let mut buf = [0u8; 512];
        loop {
            if let Some(frame) = self.deframer.next_frame() {
                let skipped = self.deframer.take_skipped();
                if skipped > 0 {
                    debug!(skipped, "skipped bytes that weren't part of a frame");
                }
                return Ok(Some(frame));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.link.read(&mut buf) {
                Ok(0) => bail!("the radio closed the connection"),
                Ok(n) => self.deframer.push(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e).context("reading from the radio"),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::VecDeque;
    use std::io;

    use meshcore_proto::companion::command;

    use super::*;

    /// A companion radio simulated at the byte level. It answers commands the
    /// way the firmware does and hands its output back a few bytes at a time.
    #[derive(Default)]
    pub(crate) struct FakeRadio {
        pub written: Vec<u8>,
        pub output: VecDeque<u8>,
        /// Queued messages, fetched one per sync command.
        pub queue: VecDeque<Vec<u8>>,
        /// Frames pushed just before the reply to the next command.
        pub pushes_before_reply: Vec<Vec<u8>>,
        pub silent: bool,
    }

    pub(crate) fn device_info_frame() -> Vec<u8> {
        let mut f = vec![code::DEVICE_INFO, 13, 175, 40, 0, 0, 0, 0];
        f.extend(*b"14-Aug-2026\0");
        f.extend([b"Heltec V4.3 OLED".as_slice(), &[0; 24]].concat());
        f.extend([b"v1.17.1-d929643".as_slice(), &[0; 5]].concat());
        f
    }

    pub(crate) fn self_info_frame(name: &str) -> Vec<u8> {
        let mut f = vec![code::SELF_INFO, 1, 10, 22];
        f.extend([0xAB; 32]);
        f.extend([0; 12]);
        f.extend(910_525u32.to_le_bytes());
        f.extend(62_500u32.to_le_bytes());
        f.extend([7, 5]);
        f.extend(name.as_bytes());
        f
    }

    pub(crate) fn stats_frame(kind: u8) -> Vec<u8> {
        let mut f = vec![code::STATS, kind];
        f.extend(match kind {
            0 => [&4296u16.to_le_bytes()[..], &60u32.to_le_bytes(), &[0, 0, 0]].concat(),
            1 => [&(-110i16).to_le_bytes()[..], &[0xB0, 20], &[0; 8]].concat(),
            _ => [5u32, 0, 0, 0, 5, 0, 0].iter().flat_map(|n| n.to_le_bytes()).collect(),
        });
        f
    }

    impl FakeRadio {
        fn send(&mut self, frame: &[u8]) {
            self.output.push_back(b'>');
            self.output.extend((frame.len() as u16).to_le_bytes());
            self.output.extend(frame);
        }

        fn answer(&mut self, command: &[u8]) {
            if self.silent {
                return;
            }
            for push in std::mem::take(&mut self.pushes_before_reply) {
                self.send(&push);
            }
            let reply = match command[0] {
                command::DEVICE_QUERY => device_info_frame(),
                command::APP_START => self_info_frame("desk"),
                command::SYNC_NEXT_MESSAGE => {
                    self.queue.pop_front().unwrap_or_else(|| vec![code::NO_MORE_MESSAGES])
                }
                command::GET_STATS => stats_frame(command[1]),
                command::GET_DEVICE_TIME => {
                    [&[code::CURR_TIME][..], &1_789_000_000u32.to_le_bytes()].concat()
                }
                _ => vec![code::ERR, 1],
            };
            self.send(&reply);
        }

        /// Pushes a frame now, as the radio does when it hears something.
        pub(crate) fn push(&mut self, frame: &[u8]) {
            self.send(frame);
        }
    }

    impl Write for FakeRadio {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(bytes);
            while self.written.len() >= 3 && self.written[0] == b'<' {
                let len = usize::from(u16::from_le_bytes([self.written[1], self.written[2]]));
                if self.written.len() < 3 + len {
                    break;
                }
                let command: Vec<u8> = self.written.drain(..3 + len).skip(3).collect();
                self.answer(&command);
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for FakeRadio {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.output.is_empty() {
                return Err(io::Error::new(ErrorKind::TimedOut, "nothing to read"));
            }
            let n = buf.len().min(self.output.len()).min(7);
            for slot in &mut buf[..n] {
                *slot = self.output.pop_front().expect("checked above");
            }
            Ok(n)
        }
    }

    fn rx_push(n: u8) -> Vec<u8> {
        vec![code::PUSH_LOG_RX_DATA, 8, (-90i8) as u8, 0x15, 0x00, n]
    }

    fn codes(received: &[Received]) -> Vec<u8> {
        received.iter().map(|received| received.frame[0]).collect()
    }

    #[test]
    fn handshake_keeps_pushes_that_arrive_first() {
        let radio = FakeRadio {
            pushes_before_reply: vec![rx_push(1), vec![code::PUSH_NEW_ADVERT, 0]],
            ..FakeRadio::default()
        };
        let mut session = Session::start(radio).unwrap();
        assert_eq!(session.identity().info.name, "desk");
        assert_eq!(session.identity().device.model, "Heltec V4.3 OLED");
        assert_eq!(codes(&session.take_received()), [code::PUSH_LOG_RX_DATA]);
        assert!(session.take_received().is_empty());
    }

    #[test]
    fn queued_messages_are_fetched_on_connect_and_when_signalled() {
        let mut radio = FakeRadio::default();
        radio.queue.push_back(vec![
            code::CONTACT_MSG_RECV_V3,
            0,
            0,
            0,
            1,
            2,
            3,
            4,
            5,
            6,
            0,
            0,
            0,
            0,
            0,
            0,
            b'a',
        ]);
        let mut session = Session::start(radio).unwrap();
        session.poll(Duration::from_millis(1)).unwrap();
        assert_eq!(codes(&session.take_received()), [code::CONTACT_MSG_RECV_V3]);

        // Nothing waiting: polling sends no sync command.
        session.poll(Duration::from_millis(1)).unwrap();
        assert!(session.link.written.is_empty());

        session.link.push(&rx_push(2));
        session.link.push(&[code::PUSH_MSG_WAITING]);
        session.link.queue.push_back(vec![
            code::CHANNEL_MSG_RECV_V3,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            b'x',
        ]);
        session.poll(Duration::from_millis(1)).unwrap();
        assert_eq!(
            codes(&session.take_received()),
            [code::PUSH_LOG_RX_DATA, code::CHANNEL_MSG_RECV_V3]
        );
    }

    #[test]
    fn stats_and_clock() {
        let mut session = Session::start(FakeRadio::default()).unwrap();
        let stats = session.stats().unwrap();
        assert_eq!(stats.iter().map(|f| f[1]).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(session.device_time().unwrap(), 1_789_000_000);
    }

    #[test]
    fn a_silent_radio_times_out() {
        let radio = FakeRadio { silent: true, ..FakeRadio::default() };
        let error = Session::start_with(radio, Duration::from_millis(20)).err().unwrap();
        assert!(error.to_string().contains("no reply to command 22"), "{error:#}");
    }
}
