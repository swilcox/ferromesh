//! One connection to a companion radio: the handshake, then commands and the
//! pushes that arrive around them.
//!
//! The radio transmits only when [`Session::send_channel`] or
//! [`Session::send_direct`] asks it to, and on its own to acknowledge direct
//! messages.

use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use meshcore_proto::companion::{
    self, ChannelInfo, Contact, Deframer, DeviceInfo, Frame, LoggedIn, SelfInfo, Sent, StatsKind,
    autoadd, code, error,
};
use tracing::{debug, info};

/// How long a command may take to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// More queued messages than the radio can hold, so a runaway loop stops.
const MAX_DRAIN: usize = 64;
const APP_NAME: &str = "ferromeshd";

fn ok_or_refusal(reply: &[u8], doing: &str) -> Result<(), Refusal> {
    match Frame::parse(reply) {
        Ok(Frame::Ok) => Ok(()),
        _ => Err(refusal(reply, doing)),
    }
}

fn refusal(reply: &[u8], doing: &str) -> Refusal {
    match Frame::parse(reply) {
        Ok(Frame::Err(Some(code))) => {
            format!("the radio refused {doing}: {}", error::describe(code))
        }
        Ok(Frame::Err(None)) => format!("the radio refused {doing}"),
        _ => format!("unexpected reply {:#04x} while {doing}", reply[0]),
    }
}

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
    /// The radio's channel slots, read on first use.
    slots: Option<Vec<ChannelInfo>>,
    /// Login answers that arrived while we were doing something else.
    logins: Vec<LoginResult>,
}

/// What a node said when we asked to log in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginResult {
    /// The first 6 bytes of the node's key, when the firmware sends them.
    pub pubkey_prefix: Option<[u8; 6]>,
    /// `None` when it refused.
    pub accepted: Option<LoggedIn>,
}

/// Why the radio didn't do what was asked. Unlike an `Err` from these
/// methods, it leaves the connection usable.
pub type Refusal = String;

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
                manual_add_contacts: 0,
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
            slots: None,
            logins: Vec::new(),
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

    #[cfg(test)]
    pub(crate) fn link(&self) -> &L {
        &self.link
    }

    #[cfg(test)]
    pub(crate) fn link_mut(&mut self) -> &mut L {
        &mut self.link
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

    /// Sets the radio's clock; it refuses to move backwards.
    pub fn set_device_time(&mut self, secs: u32) -> Result<Result<(), Refusal>> {
        let reply = self.request(&companion::set_device_time(secs))?;
        Ok(ok_or_refusal(&reply, "setting the clock"))
    }

    /// Asks to join a room server, or to administer a repeater. The radio
    /// answers at once with how long the exchange may take; the node's own
    /// answer arrives later as a push, which [`Session::take_logins`]
    /// collects.
    pub fn send_login(
        &mut self,
        pubkey: &[u8; 32],
        password: &str,
    ) -> Result<Result<Sent, Refusal>> {
        let reply = self.request(&companion::send_login(pubkey, password))?;
        Ok(match Frame::parse(&reply)? {
            Frame::Sent(sent) => Ok(sent),
            _ => Err(refusal(&reply, "logging in")),
        })
    }

    /// Login answers since the last call: whether we got in, and the key
    /// prefix of whoever answered when the firmware says.
    pub fn take_logins(&mut self) -> Vec<LoginResult> {
        std::mem::take(&mut self.logins)
    }

    /// Advertises the radio, so other nodes can add it as a contact. A
    /// flood advert crosses the mesh; a zero-hop one reaches only the
    /// radios that hear it directly.
    pub fn send_advert(&mut self, flood: bool) -> Result<Result<(), Refusal>> {
        let reply = self.request(&companion::send_self_advert(flood))?;
        Ok(ok_or_refusal(&reply, "advertising"))
    }

    /// Transmits `text` on the channel with `secret`, first giving the
    /// channel one of the radio's slots if it has none.
    pub fn send_channel(
        &mut self,
        name: &str,
        secret: &[u8; 16],
        timestamp: u32,
        text: &str,
    ) -> Result<Result<(), Refusal>> {
        let slot = match self.channel_slot(name, secret)? {
            Ok(slot) => slot,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let reply = self.request(&companion::send_channel_text(slot, timestamp, text))?;
        Ok(ok_or_refusal(&reply, "sending"))
    }

    /// Transmits a direct message, first adding the recipient as a contact
    /// if the radio doesn't know it.
    pub fn send_direct(
        &mut self,
        contact: &Contact,
        timestamp: u32,
        text: &str,
    ) -> Result<Result<Sent, Refusal>> {
        // Whoever you write to is kept for the replies.
        if let Err(refusal) = self.set_favourite(contact, true)? {
            return Ok(Err(refusal));
        }
        let prefix: [u8; 6] = contact.pubkey[..6].try_into().expect("6 bytes");
        let reply = self.request(&companion::send_text(&prefix, 0, timestamp, text))?;
        Ok(match Frame::parse(&reply)? {
            Frame::Sent(sent) => Ok(sent),
            _ => Err(refusal(&reply, "sending")),
        })
    }

    /// The contact with this key, if the radio has it.
    pub fn get_contact(&mut self, pubkey: &[u8; 32]) -> Result<Result<Option<Contact>, Refusal>> {
        let reply = self.request(&companion::get_contact(pubkey))?;
        Ok(match Frame::parse(&reply)? {
            Frame::Contact(contact) => Ok(Some(contact)),
            Frame::Err(Some(error::NOT_FOUND)) => Ok(None),
            _ => Err(refusal(&reply, "looking up the contact")),
        })
    }

    /// Adds `contact` if the radio doesn't have it. Existing contacts keep
    /// what the radio knows of them, such as their route.
    pub fn add_contact(&mut self, contact: &Contact) -> Result<Result<bool, Refusal>> {
        if self.get_contact(&contact.pubkey)?.is_ok_and(|held| held.is_some()) {
            return Ok(Ok(false));
        }
        let reply = self.request(&companion::add_update_contact(contact))?;
        Ok(ok_or_refusal(&reply, "adding the contact").map(|()| true))
    }

    /// Makes `contact` a favourite, which the radio never replaces to make
    /// room, or stops it being one, adding it first if the radio doesn't
    /// have it. Returns the contact as the radio now holds it.
    pub fn set_favourite(
        &mut self,
        contact: &Contact,
        favourite: bool,
    ) -> Result<Result<Contact, Refusal>> {
        let (mut held, known) = match self.get_contact(&contact.pubkey)? {
            Ok(Some(held)) => (held, true),
            Ok(None) => (contact.clone(), false),
            Err(refusal) => return Ok(Err(refusal)),
        };
        let flags = if favourite {
            held.flags | Contact::FAVOURITE
        } else {
            held.flags & !Contact::FAVOURITE
        };
        if known && flags == held.flags {
            return Ok(Ok(held));
        }
        held.flags = flags;
        let reply = self.request(&companion::add_update_contact(&held))?;
        let doing = if known { "updating the contact" } else { "adding the contact" };
        if let Err(refusal) = ok_or_refusal(&reply, doing) {
            return Ok(Err(refusal));
        }
        if !known {
            info!(contact = %held.name, "added a contact to the companion radio");
        }
        Ok(Ok(held))
    }

    /// Every contact on the radio.
    pub fn list_contacts(&mut self) -> Result<Vec<Contact>> {
        let reply = self.request(&companion::get_contacts())?;
        let Frame::ContactsStart(count) = Frame::parse(&reply)? else {
            bail!("unexpected reply {:#04x} to the contact list request", reply[0]);
        };
        let mut contacts = Vec::with_capacity(count as usize);
        loop {
            let frame = self.next_reply("listing contacts")?;
            match Frame::parse(&frame)? {
                Frame::Contact(contact) => contacts.push(contact),
                Frame::EndOfContacts => return Ok(contacts),
                _ => bail!("unexpected frame {:#04x} while listing contacts", frame[0]),
            }
        }
    }

    /// Chat radios only are added as contacts when heard, and a full table
    /// replaces the contact heard from least recently, unless it's a
    /// favourite. Settings are written only if they differ; returns whether
    /// they did.
    pub fn apply_contact_policy(&mut self) -> Result<Result<bool, Refusal>> {
        let wanted = autoadd::OVERWRITE_OLDEST | autoadd::CHAT;
        let reply = self.request(&companion::get_autoadd_config())?;
        let Frame::AutoAddConfig { policy, max_hops } = Frame::parse(&reply)? else {
            return Ok(Err(refusal(&reply, "reading the auto-add policy")));
        };
        let mut changed = false;
        // The policy first, so turning on selective adding never leaves a
        // moment when nobody would be added.
        if policy != wanted {
            let reply = self.request(&companion::set_autoadd_config(wanted, max_hops))?;
            if let Err(refusal) = ok_or_refusal(&reply, "setting the auto-add policy") {
                return Ok(Err(refusal));
            }
            changed = true;
        }
        if self.identity.info.manual_add_contacts & 1 == 0 {
            let reply = self.request(&companion::set_manual_add_contacts(true))?;
            if let Err(refusal) = ok_or_refusal(&reply, "limiting which contacts are added") {
                return Ok(Err(refusal));
            }
            self.identity.info.manual_add_contacts = 1;
            changed = true;
        }
        Ok(Ok(changed))
    }

    /// The next frame that isn't a push, for commands that answer with
    /// several.
    fn next_reply(&mut self, doing: &str) -> Result<Vec<u8>> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let Some(frame) = self.read_frame(deadline)? else {
                bail!("the radio stopped answering while {doing}");
            };
            if !code::is_push(frame[0]) {
                return Ok(frame);
            }
            self.push(frame);
        }
    }

    /// The slot holding `secret`, putting the channel in a free slot if none
    /// does. Slot 0 is the radio's public channel and is left alone.
    fn channel_slot(&mut self, name: &str, secret: &[u8; 16]) -> Result<Result<u8, Refusal>> {
        if self.slots.is_none() {
            let mut slots = Vec::new();
            for slot in 0..self.identity.device.max_channels {
                let reply = self.request(&companion::get_channel(slot))?;
                match Frame::parse(&reply)? {
                    Frame::ChannelInfo(info) => slots.push(info),
                    _ => break,
                }
            }
            self.slots = Some(slots);
        }
        let slots = self.slots.as_mut().expect("loaded above");
        if let Some(found) = slots.iter().find(|info| info.secret == *secret) {
            return Ok(Ok(found.slot));
        }
        let Some(free) = slots.iter().position(|info| info.slot != 0 && info.is_empty()) else {
            return Ok(Err(format!("all {} of the radio's channel slots are in use", slots.len())));
        };
        let slot = slots[free].slot;
        let reply = self.request(&companion::set_channel(slot, name, secret))?;
        if let Err(refusal) = ok_or_refusal(&reply, "setting up the channel") {
            return Ok(Err(refusal));
        }
        let slots = self.slots.as_mut().expect("loaded above");
        slots[free] = ChannelInfo { slot, name: name.to_owned(), secret: *secret };
        info!(channel = %name, slot, "gave a channel a slot on the companion radio");
        Ok(Ok(slot))
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
            code::PUSH_LOG_RX_DATA | code::PUSH_SEND_CONFIRMED => {
                self.received.push(Received { at: Timestamp::now(), frame });
            }
            code::PUSH_MSG_WAITING => self.waiting = true,
            code::PUSH_LOGIN_SUCCESS | code::PUSH_LOGIN_FAILED => match Frame::parse(&frame) {
                Ok(Frame::LoggedIn(accepted)) => self.logins.push(LoginResult {
                    pubkey_prefix: accepted.pubkey_prefix,
                    accepted: Some(accepted),
                }),
                Ok(Frame::LoginFailed(pubkey_prefix)) => {
                    self.logins.push(LoginResult { pubkey_prefix, accepted: None });
                }
                _ => {}
            },
            code::PUSH_CONTACTS_FULL => {
                info!(
                    "companion radio's contact table is full; its oldest contact will be replaced"
                );
            }
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
        /// Refuse to add contacts, as a full contact table does.
        pub refuse_contacts: bool,
        /// Frames sent right after the next reply, such as a contact list.
        pub pushes_after_reply: Vec<Vec<u8>>,
        pub autoadd: u8,
        /// Every command received, in order.
        pub commands: Vec<Vec<u8>>,
        /// Channel slots that have been set; slot 0 holds the public channel.
        pub slots: Vec<(u8, Vec<u8>)>,
        pub contacts: Vec<Vec<u8>>,
    }

    /// The radio's frame for a contact added by `stored`, an add command.
    fn contact_frame(stored: &[u8]) -> Vec<u8> {
        [&[code::CONTACT][..], &stored[1..], &0u32.to_le_bytes()].concat()
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
            self.commands.push(command.to_vec());
            let ok = vec![code::OK];
            let reply = match command[0] {
                command::GET_CHANNEL => {
                    let slot = command[1];
                    let set = self.slots.iter().find(|(s, _)| *s == slot).map(|(_, f)| f.clone());
                    let mut frame = vec![code::CHANNEL_INFO, slot];
                    match (slot, set) {
                        (_, Some(name_and_secret)) => frame.extend(name_and_secret),
                        (0, None) => {
                            frame.extend([b"Public".as_slice(), &[0; 26], &[0x8B; 16]].concat())
                        }
                        (_, None) => frame.extend([0; 48]),
                    }
                    frame
                }
                command::SET_CHANNEL => {
                    self.slots.push((command[1], command[2..].to_vec()));
                    ok
                }
                command::SEND_CHANNEL_TXT_MSG
                | command::SET_DEVICE_TIME
                | command::SEND_SELF_ADVERT => ok,
                // Contacts are kept as the frames that added them, which have
                // the same layout as the radio's contact frames.
                command::GET_CONTACT_BY_KEY => {
                    match self.contacts.iter().find(|stored| stored[1..33] == command[1..33]) {
                        Some(stored) => contact_frame(stored),
                        None => vec![code::ERR, 2],
                    }
                }
                command::ADD_UPDATE_CONTACT if self.refuse_contacts => vec![code::ERR, 3],
                command::ADD_UPDATE_CONTACT => {
                    self.contacts.retain(|stored| stored[1..33] != command[1..33]);
                    self.contacts.push(command.to_vec());
                    ok
                }
                command::GET_CONTACTS => {
                    let count = self.contacts.len() as u32;
                    for stored in self.contacts.clone() {
                        self.pushes_after_reply.push(contact_frame(&stored));
                    }
                    self.pushes_after_reply.push(vec![code::END_OF_CONTACTS, 0, 0, 0, 0]);
                    [&[code::CONTACTS_START][..], &count.to_le_bytes()].concat()
                }
                command::GET_AUTOADD_CONFIG => vec![code::AUTOADD_CONFIG, self.autoadd, 0],
                command::SET_AUTOADD_CONFIG => {
                    self.autoadd = command[1];
                    ok
                }
                command::SET_OTHER_PARAMS => ok,
                command::SEND_TXT_MSG | command::SEND_LOGIN => {
                    [&[code::SENT, 1][..], &0xACu32.to_le_bytes(), &3000u32.to_le_bytes()].concat()
                }
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
            for frame in std::mem::take(&mut self.pushes_after_reply) {
                self.send(&frame);
            }
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

    fn contact(seed: u8, name: &str) -> Contact {
        Contact {
            pubkey: [seed; 32],
            kind: 1,
            flags: 0,
            out_path_len: Some(2),
            out_path: vec![0xAB, 0xCD],
            name: name.into(),
            last_advert: 5,
            lat_e6: 0,
            lon_e6: 0,
        }
    }

    #[test]
    fn favourites_keep_what_the_radio_knows() {
        let mut session = Session::start(FakeRadio::default()).unwrap();
        let added = session.set_favourite(&contact(1, "KK4SW"), true).unwrap().unwrap();
        assert!(added.is_favourite());
        assert_eq!(session.add_contact(&contact(1, "KK4SW")).unwrap(), Ok(false), "already there");
        assert_eq!(session.add_contact(&contact(2, "Hilltop")).unwrap(), Ok(true));

        let listed = session.list_contacts().unwrap();
        let summary: Vec<_> = listed.iter().map(|c| (c.name.as_str(), c.is_favourite())).collect();
        assert_eq!(summary, [("KK4SW", true), ("Hilltop", false)]);
        // The stored route comes back whole, to be written back unchanged.
        assert_eq!(
            (listed[0].out_path_len, &listed[0].out_path[..3]),
            (Some(2), &[0xAB, 0xCD, 0][..])
        );

        let unpinned = session.set_favourite(&contact(1, "ignored"), false).unwrap().unwrap();
        assert_eq!((unpinned.name.as_str(), unpinned.is_favourite()), ("KK4SW", false));
        assert_eq!(session.get_contact(&[9; 32]).unwrap(), Ok(None));
    }

    #[test]
    fn the_contact_policy_is_written_once() {
        let mut session = Session::start(FakeRadio::default()).unwrap();
        assert_eq!(session.apply_contact_policy().unwrap(), Ok(true));
        let writes: Vec<Vec<u8>> = session
            .link
            .commands
            .iter()
            .filter(|c| matches!(c[0], command::SET_AUTOADD_CONFIG | command::SET_OTHER_PARAMS))
            .cloned()
            .collect();
        assert_eq!(
            writes,
            [vec![command::SET_AUTOADD_CONFIG, 0x03, 0], vec![command::SET_OTHER_PARAMS, 1]]
        );
        assert_eq!(session.apply_contact_policy().unwrap(), Ok(false));
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
