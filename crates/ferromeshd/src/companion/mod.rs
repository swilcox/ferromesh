//! The companion radio source: a radio running MeshCore's stock companion
//! firmware on a serial port. ferromeshd must be its only client, because
//! whichever client fetches a queued message takes it.
//!
//! Every packet the radio hears becomes an observation under the radio's own
//! key, beside the MQTT observers', and direct messages it decrypts are
//! stored. Like MQTT messages, everything is written to the raw log first.

pub mod link;
pub mod record;
pub mod session;

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use jiff::Timestamp;
use meshcore_proto::companion::{Frame, Stats};
use tokio::sync::mpsc;
use tracing::{info, warn};

use self::session::Session;
use crate::config::CompanionConfig;
use crate::rawlog::RawRecord;
use crate::writer::Job;

pub const TOPIC_PREFIX: &str = "companion/";

const RETRY: Duration = Duration::from_secs(5);
/// How long each poll waits for pushes before checking for stop and status.
const POLL: Duration = Duration::from_secs(1);
const STATUS_EVERY: Duration = Duration::from_secs(300);
/// A radio can come up unable to hear anything while looking healthy.
const STALL: Duration = Duration::from_secs(900);
/// A clock further off than this would put wrong times on sent messages.
const CLOCK_TOLERANCE_SECS: i64 = 120;

/// The thread that owns the radio.
pub struct Companion {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Companion {
    pub fn spawn(config: CompanionConfig, jobs: mpsc::Sender<Job>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("companion".into())
            .spawn(move || run(&config.device, &jobs, &flag))?;
        Ok(Self { stop, thread })
    }

    /// Closes the port and waits for the thread, which drops its handle on
    /// the writer.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

/// Connects, reconnecting after failures, until stopped.
fn run(device: &str, jobs: &mpsc::Sender<Job>, stop: &AtomicBool) {
    let mut last_error = None;
    while !stop.load(Ordering::Relaxed) {
        match connect(device, jobs, stop, &mut last_error) {
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
    let drift = i64::from(session.device_time()?) - Timestamp::now().as_second();
    if drift.abs() > CLOCK_TOLERANCE_SECS {
        warn!(
            drift_secs = drift,
            "companion radio's clock is off; messages it sends would carry the wrong time"
        );
    }
    serve(&mut session, &format!("companion:{path}"), jobs, stop, POLL)
}

/// Forwards what the radio hears, with a status report every few minutes.
fn serve<L: Read + Write>(
    session: &mut Session<L>,
    source: &str,
    jobs: &mpsc::Sender<Job>,
    stop: &AtomicBool,
    poll: Duration,
) -> Result<()> {
    let identity = session.identity().clone();
    let mut health = Health::default();
    let mut next_status = Instant::now();
    loop {
        if Instant::now() >= next_status {
            let frames = session.stats()?;
            health.check(&frames);
            if !send(jobs, record::status(&identity, source, Timestamp::now(), &frames)) {
                return Ok(());
            }
            next_status += STATUS_EVERY;
        }
        session.poll(poll)?;
        for received in session.take_received() {
            if let Some(record) = record::received(&identity, source, &received)
                && !send(jobs, record)
            {
                return Ok(());
            }
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
    }
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
    use meshcore_proto::companion::code;

    use super::session::tests::FakeRadio;
    use super::*;

    #[test]
    fn serve_forwards_status_receptions_and_messages() {
        let mut radio = FakeRadio {
            pushes_before_reply: vec![vec![code::PUSH_LOG_RX_DATA, 8, 0xA6, 0x15, 0x00, 1]],
            ..FakeRadio::default()
        };
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

        let (jobs, mut queue) = mpsc::channel(16);
        let stop = AtomicBool::new(true);
        serve(&mut session, "companion:test", &jobs, &stop, Duration::from_millis(1)).unwrap();

        let mut topics = Vec::new();
        while let Ok(Job::Record(record)) = queue.try_recv() {
            assert_eq!(record.source, "companion:test");
            topics.push(record.topic.rsplit('/').next().unwrap().to_owned());
        }
        assert_eq!(topics, ["status", "rx", "message"]);
    }

    #[test]
    fn serve_stops_when_the_writer_is_gone() {
        let mut session = Session::start(FakeRadio::default()).unwrap();
        let (jobs, queue) = mpsc::channel(16);
        drop(queue);
        let stop = AtomicBool::new(false);
        serve(&mut session, "companion:test", &jobs, &stop, Duration::from_millis(1)).unwrap();
    }
}
