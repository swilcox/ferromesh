//! The companion radio source: a radio running MeshCore's stock companion
//! firmware on a serial port. ferromeshd must be its only client, because
//! whichever client fetches a queued message takes it.
//!
//! Every packet the radio hears becomes an observation under the radio's own
//! key, beside the MQTT observers', and direct messages it decrypts are
//! stored. Messages can be sent through it with [`Request`]s, and its
//! contact list is kept to the nodes you talk to (see [`contacts`]). Like
//! MQTT messages, everything is written to the raw log first.

pub mod contacts;
pub mod link;
pub mod record;
pub mod session;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use jiff::Timestamp;
use meshcore_proto::companion::{Contact, Frame, MAX_TEXT_LEN, Stats};
use meshcore_proto::{ChannelKey, GroupText, PacketHash, PayloadType};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use self::contacts::{Database, Directory, Rescue};
use self::session::{Identity, Session};
use crate::config::CompanionConfig;
use crate::rawlog::RawRecord;
use crate::writer::Job;

pub const TOPIC_PREFIX: &str = "companion/";

const RETRY: Duration = Duration::from_secs(5);
/// How long each poll waits for pushes before checking for requests, stop
/// and status.
const POLL: Duration = Duration::from_millis(250);
const STATUS_EVERY: Duration = Duration::from_secs(300);
/// A radio can come up unable to hear anything while looking healthy.
const STALL: Duration = Duration::from_secs(900);
/// A clock further off than this would put wrong times on sent messages.
const CLOCK_TOLERANCE_SECS: i64 = 120;

/// Something for the radio to do.
pub enum Request {
    Channel {
        name: String,
        secret: [u8; 16],
        text: String,
        reply: Reply,
    },
    Direct {
        contact: Contact,
        text: String,
        reply: Reply,
    },
    /// List the radio's contacts.
    Contacts {
        reply: oneshot::Sender<Result<Vec<Contact>, SendError>>,
    },
    /// Make a contact a favourite, adding it if needed, or stop it being one.
    Pin {
        contact: Contact,
        pinned: bool,
        reply: oneshot::Sender<Result<Contact, SendError>>,
    },
}

pub type Reply = oneshot::Sender<Result<Accepted, SendError>>;

/// The radio took the message. It's in the outbox under this timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    pub sender_timestamp: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The message can't be sent as written, such as when it's too long.
    Invalid(String),
    /// The radio isn't connected.
    Unavailable(String),
    /// The radio refused it, or failed while sending; it's in the outbox.
    Failed(String),
}

impl Request {
    fn refuse(self, error: SendError) {
        match self {
            Self::Channel { reply, .. } | Self::Direct { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Contacts { reply } => {
                let _ = reply.send(Err(error));
            }
            Self::Pin { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

/// The thread that owns the radio.
pub struct Companion {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    requests: std_mpsc::Sender<Request>,
}

impl Companion {
    /// `db_path` is the server's database, for looking up nodes that have
    /// advertised.
    pub fn spawn(
        config: CompanionConfig,
        jobs: mpsc::Sender<Job>,
        db_path: PathBuf,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (requests, queue) = std_mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("companion".into())
            .spawn(move || run(&config.device, &jobs, &queue, &Database(db_path), &flag))?;
        Ok(Self { stop, thread, requests })
    }

    /// Where to send [`Request`]s.
    pub fn requests(&self) -> std_mpsc::Sender<Request> {
        self.requests.clone()
    }

    /// Closes the port and waits for the thread, which drops its handle on
    /// the writer.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

/// Connects, reconnecting after failures, until stopped.
fn run(
    device: &str,
    jobs: &mpsc::Sender<Job>,
    requests: &std_mpsc::Receiver<Request>,
    directory: &dyn Directory,
    stop: &AtomicBool,
) {
    let mut last_error = None;
    while !stop.load(Ordering::Relaxed) {
        match connect(device, jobs, requests, directory, stop, &mut last_error) {
            Ok(()) => return,
            Err(e) => {
                // Say it once, not every few seconds while the radio is away.
                let message = format!("{e:#}");
                if last_error.as_ref() != Some(&message) {
                    warn!("companion radio: {message}; retrying every {}s", RETRY.as_secs());
                    last_error = Some(message);
                }
                let until = Instant::now() + RETRY;
                while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                    while let Ok(request) = requests.try_recv() {
                        request.refuse(SendError::Unavailable(
                            "the companion radio isn't connected".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

/// One connection. Returns `Ok` once stopped, or if the writer has gone.
fn connect(
    device: &str,
    jobs: &mpsc::Sender<Job>,
    requests: &std_mpsc::Receiver<Request>,
    directory: &dyn Directory,
    stop: &AtomicBool,
    last_error: &mut Option<String>,
) -> Result<()> {
    let (path, port) = link::open(device)?;
    let mut session =
        Session::start(port).with_context(|| format!("no companion radio answered on {path}"))?;
    *last_error = None;
    let identity = session.identity();
    info!(
        port = %path,
        name = %identity.info.name,
        key = %hex::encode(&identity.info.pubkey[..4]),
        model = %identity.device.model,
        firmware = %identity.device.version,
        radio = %identity.info.radio(),
        "companion radio connected"
    );
    check_clock(&mut session)?;
    match session.apply_contact_policy()? {
        Ok(true) => info!(
            "companion radio now adds only chat radios as contacts, replacing its oldest when full"
        ),
        Ok(false) => {}
        Err(refusal) => warn!("couldn't set the companion radio's contact policy: {refusal}"),
    }
    serve(&mut session, &format!("companion:{path}"), jobs, requests, directory, stop, POLL)
}

/// Sets the radio's clock if it's behind; the firmware can't move it back.
fn check_clock<L: Read + Write>(session: &mut Session<L>) -> Result<()> {
    let now = Timestamp::now().as_second();
    let drift = i64::from(session.device_time()?) - now;
    if drift < -CLOCK_TOLERANCE_SECS {
        match session.set_device_time(now as u32)? {
            Ok(()) => info!(behind_secs = -drift, "set the companion radio's clock"),
            Err(refusal) => {
                warn!(behind_secs = -drift, "couldn't set the companion radio's clock: {refusal}")
            }
        }
    } else if drift > CLOCK_TOLERANCE_SECS {
        warn!(
            ahead_secs = drift,
            "companion radio's clock is ahead and can't be moved back; messages it sends will carry a later time"
        );
    }
    Ok(())
}

/// Forwards what the radio hears and sends what's asked, with a status
/// report every few minutes.
fn serve<L: Read + Write>(
    session: &mut Session<L>,
    source: &str,
    jobs: &mpsc::Sender<Job>,
    requests: &std_mpsc::Receiver<Request>,
    directory: &dyn Directory,
    stop: &AtomicBool,
    poll: Duration,
) -> Result<()> {
    let identity = session.identity().clone();
    let mut health = Health::default();
    let mut rescue = Rescue::default();
    let mut next_status = Instant::now();
    let mut last_timestamp = 0;
    loop {
        if Instant::now() >= next_status {
            let frames = session.stats()?;
            health.check(&frames);
            if !send(jobs, record::status(&identity, source, Timestamp::now(), &frames)) {
                return Ok(());
            }
            next_status += STATUS_EVERY;
        }
        while let Ok(request) = requests.try_recv() {
            let timestamp = next_timestamp(&mut last_timestamp);
            handle(session, &identity, source, jobs, request, timestamp)?;
        }
        session.poll(poll)?;
        for received in session.take_received() {
            match Frame::parse(&received.frame) {
                Ok(Frame::RxLog(rx)) => {
                    rescue.heard(rx.raw, identity.info.pubkey[0], Instant::now())
                }
                Ok(Frame::ContactMessage(message)) => {
                    rescue.delivered(message.sender_prefix[0]);
                    contacts::pin_sender(session, directory, &message.sender_prefix)?;
                }
                _ => {}
            }
            if let Some(record) = record::received(&identity, source, &received)
                && !send(jobs, record)
            {
                return Ok(());
            }
        }
        for sender in rescue.due(Instant::now()) {
            contacts::rescue(session, directory, sender)?;
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
    }
}

/// Sends one request and records the outcome. Returns an error only when the
/// connection failed, after telling the requester.
fn handle<L: Read + Write>(
    session: &mut Session<L>,
    identity: &Identity,
    source: &str,
    jobs: &mpsc::Sender<Job>,
    request: Request,
    timestamp: u32,
) -> Result<()> {
    match request {
        Request::Channel { name, secret, text, reply } => {
            // The radio sends `<its name>: <text>`, and that counts too.
            let limit = MAX_TEXT_LEN.saturating_sub(identity.info.name.len() + 2);
            if let Err(problem) = check_text(&text, limit) {
                let _ = reply.send(Err(SendError::Invalid(problem)));
                return Ok(());
            }
            let packet_hash = channel_packet_hash(&identity.info.name, &secret, timestamp, &text);
            let result = session.send_channel(&name, &secret, timestamp, &text);
            let outcome = flatten(&result);
            let error = outcome.as_ref().err().map(String::as_str);
            send(
                jobs,
                record::sent_channel(
                    identity,
                    source,
                    Timestamp::now(),
                    &name,
                    &text,
                    timestamp,
                    &packet_hash,
                    error,
                ),
            );
            let _ = reply.send(
                outcome
                    .map(|()| Accepted { sender_timestamp: timestamp })
                    .map_err(SendError::Failed),
            );
            result.map(|_| ())
        }
        Request::Contacts { reply } => {
            let result = session.list_contacts();
            let _ = reply.send(
                result.as_ref().map(Clone::clone).map_err(|e| SendError::Failed(format!("{e:#}"))),
            );
            result.map(|_| ())
        }
        Request::Pin { contact, pinned, reply } => {
            let result = session.set_favourite(&contact, pinned);
            let _ = reply.send(flatten(&result).map_err(SendError::Failed));
            result.map(|_| ())
        }
        Request::Direct { contact, text, reply } => {
            if let Err(problem) = check_text(&text, MAX_TEXT_LEN) {
                let _ = reply.send(Err(SendError::Invalid(problem)));
                return Ok(());
            }
            let result = session.send_direct(&contact, timestamp, &text);
            let outcome = flatten(&result);
            let recorded = outcome.as_ref().map_err(String::as_str);
            send(
                jobs,
                record::sent_direct(
                    identity,
                    source,
                    Timestamp::now(),
                    &contact,
                    &text,
                    timestamp,
                    recorded,
                ),
            );
            let _ = reply.send(
                outcome
                    .map(|_| Accepted { sender_timestamp: timestamp })
                    .map_err(SendError::Failed),
            );
            result.map(|_| ())
        }
    }
}

/// A connection failure and a refusal both mean the message wasn't sent.
fn flatten<T: Clone>(result: &Result<Result<T, String>>) -> Result<T, String> {
    match result {
        Ok(Ok(value)) => Ok(value.clone()),
        Ok(Err(refusal)) => Err(refusal.clone()),
        Err(e) => Err(format!("{e:#}")),
    }
}

fn check_text(text: &str, limit: usize) -> Result<(), String> {
    if text.trim().is_empty() {
        Err("the message is empty".into())
    } else if text.contains('\0') {
        Err("the message contains a NUL character".into())
    } else if text.len() > limit {
        Err(format!("the message is {} bytes; at most {limit} fit", text.len()))
    } else {
        Ok(())
    }
}

/// The hash the radio's channel packet will have, which is what observers
/// report hearing: the same plaintext, encrypted the same way.
fn channel_packet_hash(radio_name: &str, secret: &[u8; 16], timestamp: u32, text: &str) -> [u8; 8] {
    let plaintext = GroupText {
        sender_timestamp: timestamp,
        txt_type: 0,
        attempt: 0,
        text: format!("{radio_name}: {text}").into_bytes(),
    };
    let key = ChannelKey::from_secret(secret).expect("a 16-byte secret is valid");
    let payload = key.encrypt(&plaintext.to_plaintext());
    PacketHash::compute(PayloadType::GrpTxt, 0, &payload).0
}

/// Seconds since the epoch, but always later than the last, so every send
/// is unique.
fn next_timestamp(last: &mut u32) -> u32 {
    let now = u32::try_from(Timestamp::now().as_second()).unwrap_or(u32::MAX);
    *last = now.max(last.saturating_add(1));
    *last
}

/// False once the writer has stopped.
fn send(jobs: &mpsc::Sender<Job>, record: RawRecord) -> bool {
    jobs.blocking_send(Job::Record(record)).is_ok()
}

/// Warns when the radio's received-packet count stops moving.
#[derive(Default)]
struct Health {
    received: Option<u32>,
    since: Option<Instant>,
    warned: bool,
}

impl Health {
    fn check(&mut self, frames: &[Vec<u8>]) {
        let Some(received) = frames.iter().find_map(|frame| match Frame::parse(frame) {
            Ok(Frame::Stats(Stats::Packets { received, .. })) => Some(received),
            _ => None,
        }) else {
            return;
        };
        if self.received != Some(received) {
            if self.warned {
                info!("companion radio is hearing packets again");
            }
            *self = Self { received: Some(received), since: Some(Instant::now()), warned: false };
        } else if !self.warned && self.since.is_some_and(|since| since.elapsed() >= STALL) {
            warn!(
                minutes = STALL.as_secs() / 60,
                "companion radio has heard nothing; if other observers hear traffic, reboot it"
            );
            self.warned = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use meshcore_proto::companion::{code, command};
    use meshcore_proto::{Packet, Payload};

    use super::session::tests::FakeRadio;
    use super::*;

    fn records(queue: &mut mpsc::Receiver<Job>) -> Vec<RawRecord> {
        let mut records = Vec::new();
        while let Ok(job) = queue.try_recv() {
            if let Job::Record(record) = job {
                records.push(record);
            }
        }
        records
    }

    fn kinds(records: &[RawRecord]) -> Vec<&str> {
        records.iter().map(|record| record.topic.rsplit('/').next().unwrap()).collect()
    }

    fn contact() -> Contact {
        Contact {
            pubkey: [3; 32],
            kind: 1,
            flags: 0,
            out_path_len: None,
            out_path: Vec::new(),
            name: "KK4SW".into(),
            last_advert: 0,
            lat_e6: 0,
            lon_e6: 0,
        }
    }

    #[test]
    fn serve_forwards_status_receptions_and_messages() {
        let mut radio = FakeRadio {
            pushes_before_reply: vec![vec![code::PUSH_LOG_RX_DATA, 8, 0xA6, 0x15, 0x00, 1]],
            ..FakeRadio::default()
        };
        radio.queue.push_back(
            [
                &[code::CONTACT_MSG_RECV_V3, 0, 0, 0][..],
                &[1, 2, 3, 4, 5, 6],
                &[0, 0, 0, 0, 0, 0],
                b"a",
            ]
            .concat(),
        );
        let mut session = Session::start(radio).unwrap();

        let (jobs, mut queue) = mpsc::channel(16);
        let (_requests, waiting) = std_mpsc::channel();
        let stop = AtomicBool::new(true);
        serve(
            &mut session,
            "companion:test",
            &jobs,
            &waiting,
            &contacts::tests::Nodes(Vec::new()),
            &stop,
            Duration::from_millis(1),
        )
        .unwrap();
        let records = records(&mut queue);
        assert!(records.iter().all(|record| record.source == "companion:test"));
        assert_eq!(kinds(&records), ["status", "rx", "message"]);
    }

    #[test]
    fn serve_stops_when_the_writer_is_gone() {
        let mut session = Session::start(FakeRadio::default()).unwrap();
        let (jobs, queue) = mpsc::channel(16);
        drop(queue);
        let (_requests, waiting) = std_mpsc::channel();
        let stop = AtomicBool::new(false);
        serve(
            &mut session,
            "companion:test",
            &jobs,
            &waiting,
            &contacts::tests::Nodes(Vec::new()),
            &stop,
            Duration::from_millis(1),
        )
        .unwrap();
    }

    #[test]
    fn channel_messages_get_a_slot_and_a_predicted_hash() {
        let mut session = Session::start(FakeRadio::default()).unwrap();
        let identity = session.identity().clone();
        let (jobs, mut queue) = mpsc::channel(16);
        let (reply, answer) = oneshot::channel();
        let request =
            Request::Channel { name: "#test".into(), secret: [7; 16], text: "hi".into(), reply };
        handle(&mut session, &identity, "companion:test", &jobs, request, 1_789_000_000).unwrap();
        assert_eq!(
            answer.blocking_recv().unwrap(),
            Ok(Accepted { sender_timestamp: 1_789_000_000 })
        );

        // Slot 0 is the public channel, so the new channel took slot 1.
        let sent =
            session.link().commands.iter().find(|c| c[0] == command::SEND_CHANNEL_TXT_MSG).unwrap();
        assert_eq!(sent[2], 1);
        let set = session.link().commands.iter().find(|c| c[0] == command::SET_CHANNEL).unwrap();
        assert_eq!((set[1], &set[2..7]), (1, &b"#test"[..]));

        // The predicted hash matches the packet the firmware would build.
        let records = records(&mut queue);
        assert_eq!(kinds(&records), ["sent"]);
        let payload: serde_json::Value = serde_json::from_str(&records[0].payload).unwrap();
        let text = GroupText {
            sender_timestamp: 1_789_000_000,
            txt_type: 0,
            attempt: 0,
            text: b"desk: hi".to_vec(),
        };
        let key = ChannelKey::from_secret(&[7; 16]).unwrap();
        let frame = [&[0x15, 0x00][..], &key.encrypt(&text.to_plaintext())].concat();
        let packet = Packet::parse(&frame).unwrap();
        assert!(matches!(packet.decode_payload(), Ok(Payload::Group(_))));
        assert_eq!(payload["packet_hash"], packet.hash().to_string());

        // A second send reuses the slot without setting it again.
        let (reply, _answer) = oneshot::channel();
        let request =
            Request::Channel { name: "#test".into(), secret: [7; 16], text: "again".into(), reply };
        handle(&mut session, &identity, "companion:test", &jobs, request, 1_789_000_001).unwrap();
        let sets = session.link().commands.iter().filter(|c| c[0] == command::SET_CHANNEL).count();
        assert_eq!(sets, 1);
    }

    #[test]
    fn direct_messages_add_the_contact_and_await_its_acknowledgement() {
        let mut session = Session::start(FakeRadio::default()).unwrap();
        let identity = session.identity().clone();
        let (jobs, mut queue) = mpsc::channel(16);
        let (reply, answer) = oneshot::channel();
        let request = Request::Direct { contact: contact(), text: "hello".into(), reply };
        handle(&mut session, &identity, "companion:test", &jobs, request, 1_789_000_000).unwrap();
        assert_eq!(
            answer.blocking_recv().unwrap(),
            Ok(Accepted { sender_timestamp: 1_789_000_000 })
        );
        let order: Vec<u8> = session.link().commands.iter().map(|c| c[0]).skip(2).collect();
        assert_eq!(
            order,
            [command::GET_CONTACT_BY_KEY, command::ADD_UPDATE_CONTACT, command::SEND_TXT_MSG]
        );

        let records = records(&mut queue);
        let payload: serde_json::Value = serde_json::from_str(&records[0].payload).unwrap();
        assert_eq!(
            (payload["expected_ack"].as_u64(), payload["to_name"].as_str()),
            (Some(0xAC), Some("KK4SW"))
        );

        // The acknowledgement arrives later, as a push.
        session.link_mut().push(
            &[&[code::PUSH_SEND_CONFIRMED][..], &0xACu32.to_le_bytes(), &700u32.to_le_bytes()]
                .concat(),
        );
        session.poll(Duration::from_millis(1)).unwrap();
        let confirmed = session.take_received();
        assert_eq!(confirmed[0].frame[0], code::PUSH_SEND_CONFIRMED);
    }

    #[test]
    fn bad_text_never_reaches_the_radio() {
        let mut session = Session::start(FakeRadio::default()).unwrap();
        let identity = session.identity().clone();
        let (jobs, mut queue) = mpsc::channel(16);
        for text in ["", "  ", &"x".repeat(155)] {
            let (reply, answer) = oneshot::channel();
            let request = Request::Channel {
                name: "#test".into(),
                secret: [7; 16],
                text: text.into(),
                reply,
            };
            handle(&mut session, &identity, "companion:test", &jobs, request, 1).unwrap();
            assert!(
                matches!(answer.blocking_recv().unwrap(), Err(SendError::Invalid(_))),
                "{text:?}"
            );
        }
        // 160 bytes minus "desk: " fits exactly.
        let (reply, answer) = oneshot::channel();
        let request = Request::Channel {
            name: "#test".into(),
            secret: [7; 16],
            text: "x".repeat(154),
            reply,
        };
        handle(&mut session, &identity, "companion:test", &jobs, request, 2).unwrap();
        assert!(answer.blocking_recv().unwrap().is_ok());
        assert_eq!(kinds(&records(&mut queue)), ["sent"]);
    }

    #[test]
    fn refusals_are_recorded_and_reported() {
        let radio = FakeRadio { refuse_contacts: true, ..FakeRadio::default() };
        let mut session = Session::start(radio).unwrap();
        let identity = session.identity().clone();
        let (jobs, mut queue) = mpsc::channel(16);
        let (reply, answer) = oneshot::channel();
        let request = Request::Direct { contact: contact(), text: "hello".into(), reply };
        handle(&mut session, &identity, "companion:test", &jobs, request, 5).unwrap();
        assert_eq!(
            answer.blocking_recv().unwrap(),
            Err(SendError::Failed("the radio refused adding the contact: table full".into()))
        );
        let payload: serde_json::Value =
            serde_json::from_str(&records(&mut queue)[0].payload).unwrap();
        assert_eq!(payload["error"], "the radio refused adding the contact: table full");
    }

    #[test]
    fn timestamps_are_unique() {
        let mut last = u32::MAX - 5;
        assert_eq!(next_timestamp(&mut last), u32::MAX - 4);
        let mut last = 0;
        let first = next_timestamp(&mut last);
        assert_eq!(next_timestamp(&mut last), first + 1);
    }
}
