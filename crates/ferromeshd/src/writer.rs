//! The writer thread. Every database change happens here, one at a time, so
//! row ids follow commit order and live streams see each change exactly once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ferromesh_model::Event;
use ferromesh_store::{ChannelKind, Store};
use meshcore_proto::ChannelKey;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::info;

use crate::pipeline::{self, AddOutcome, Tally};
use crate::rawlog::{RawLogWriter, RawRecord};

/// MQTT messages that arrive together share one transaction and one fsync.
const MAX_BATCH: usize = 512;
const REPORT_EVERY: Duration = Duration::from_secs(600);

pub enum Job {
    /// An MQTT message to log raw and store.
    Record(RawRecord),
    /// Add a channel, then decrypt stored packets that were waiting for it.
    AddChannel {
        name: String,
        key: ChannelKey,
        kind: ChannelKind,
        reply: oneshot::Sender<Result<AddOutcome>>,
    },
    /// Replies once every earlier job is done.
    Sync(oneshot::Sender<()>),
}

/// Runs until every sender of `jobs` is gone. The store should come from
/// `pipeline::open_store`.
pub fn run(
    store: Store,
    raw: RawLogWriter,
    mut jobs: mpsc::Receiver<Job>,
    events: broadcast::Sender<Arc<Event>>,
) -> Result<()> {
    let mut writer = Writer { store, raw, events, records: Vec::new(), tally: Tally::default() };
    writer.store.track_changes();
    let mut last_report = Instant::now();
    while let Some(job) = jobs.blocking_recv() {
        writer.handle(job)?;
        // Take whatever else is already queued before committing.
        while writer.records.len() < MAX_BATCH {
            let Ok(job) = jobs.try_recv() else { break };
            writer.handle(job)?;
        }
        writer.flush()?;
        if last_report.elapsed() >= REPORT_EVERY {
            info!(tally = %writer.tally, "ingested");
            last_report = Instant::now();
        }
    }
    writer.flush()?;
    info!(tally = %writer.tally, "ingested");
    Ok(())
}

struct Writer {
    store: Store,
    raw: RawLogWriter,
    events: broadcast::Sender<Arc<Event>>,
    records: Vec<RawRecord>,
    tally: Tally,
}

impl Writer {
    fn handle(&mut self, job: Job) -> Result<()> {
        match job {
            Job::Record(record) => self.records.push(record),
            Job::AddChannel { name, key, kind, reply } => {
                // Store what's queued first, so the backfill covers it.
                self.flush()?;
                let outcome = pipeline::add_channel(&mut self.store, &name, &key, kind);
                if let Ok(AddOutcome::Added(added)) = &outcome {
                    info!(channel = %name, decrypted = added.backfill.decrypted, "added channel");
                }
                pipeline::publish(&mut self.store, &self.events)?;
                let _ = reply.send(outcome);
            }
            Job::Sync(reply) => {
                self.flush()?;
                let _ = reply.send(());
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.records.is_empty() {
            return Ok(());
        }
        // Raw log first: if the database write fails, a rebuild still has it.
        self.raw.append(&self.records)?;
        pipeline::ingest(&mut self.store, &self.records, &mut self.tally)?;
        pipeline::publish(&mut self.store, &self.events)?;
        self.records.clear();
        Ok(())
    }
}
