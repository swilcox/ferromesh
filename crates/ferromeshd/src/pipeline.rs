//! Raw records into the store, and new rows out to stream subscribers.
//! Shared by serve, import and rebuild.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result};
use ferromesh_model::{Event, Kind};
use ferromesh_store::{ChannelKind, Counts, Outcome, Store};
use jiff::Timestamp;
use meshcore_proto::ChannelKey;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::config::Config;
use crate::meshcoretomqtt::{self, Message};
use crate::rawlog::RawRecord;

/// Opens the database, creating it if needed, and adds configured channels.
pub fn open_store(config: &Config) -> Result<Store> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating {}", config.data_dir.display()))?;
    let path = config.db_path();
    let mut store = Store::open(&path).with_context(|| format!("opening {}", path.display()))?;
    add_configured_channels(&mut store, config)?;
    Ok(store)
}

pub fn add_configured_channels(store: &mut Store, config: &Config) -> Result<()> {
    let now = Timestamp::now().as_microsecond();
    for channel in &config.channels {
        let (key, kind) = match &channel.key {
            Some(key) => (
                ChannelKey::from_base64(key)
                    .with_context(|| format!("channel {:?}", channel.name))?,
                ChannelKind::Key,
            ),
            None => (ChannelKey::from_hashtag(&channel.name), ChannelKind::Hashtag),
        };
        if store.add_channel(&channel.name, &key, kind, now)? {
            info!(channel = %channel.name, "added channel");
        }
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Tally {
    pub records: u64,
    pub observations: u64,
    pub new_packets: u64,
    pub duplicates: u64,
    pub statuses: u64,
    pub malformed: u64,
    pub ignored: u64,
    pub unparsed: u64,
}

impl Tally {
    fn add(&mut self, other: Self) {
        self.records += other.records;
        self.observations += other.observations;
        self.new_packets += other.new_packets;
        self.duplicates += other.duplicates;
        self.statuses += other.statuses;
        self.malformed += other.malformed;
        self.ignored += other.ignored;
        self.unparsed += other.unparsed;
    }
}

impl fmt::Display for Tally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "records={} observations={} new_packets={} duplicates={} statuses={} malformed={} \
             ignored={} unparsed={}",
            self.records,
            self.observations,
            self.new_packets,
            self.duplicates,
            self.statuses,
            self.malformed,
            self.ignored,
            self.unparsed,
        )
    }
}

/// Stores `records` in one transaction, adding to `tally` once it commits.
pub fn ingest(store: &mut Store, records: &[RawRecord], tally: &mut Tally) -> Result<()> {
    let mut batch_tally = Tally::default();
    store.write(|batch| {
        for record in records {
            batch_tally.records += 1;
            match meshcoretomqtt::parse(&record.topic, &record.payload) {
                Err(e) => {
                    batch_tally.unparsed += 1;
                    warn!(topic = %record.topic, "unparseable message: {e:#}");
                }
                Ok(Message::Ignored) => batch_tally.ignored += 1,
                Ok(Message::Status(report)) => {
                    if batch.record_status(&report)? {
                        batch_tally.statuses += 1;
                    } else {
                        batch_tally.duplicates += 1;
                    }
                }
                Ok(Message::Packet(reception)) => match batch.record_reception(&reception)? {
                    Outcome::Recorded { new_packet, .. } => {
                        batch_tally.observations += 1;
                        batch_tally.new_packets += u64::from(new_packet);
                    }
                    Outcome::Duplicate => batch_tally.duplicates += 1,
                    Outcome::Malformed(e) => {
                        batch_tally.malformed += 1;
                        warn!(topic = %record.topic, "malformed frame: {e}");
                    }
                },
            }
        }
        Ok(())
    })?;
    tally.add(batch_tally);
    Ok(())
}

/// Broadcasts events for the rows stored since the last call. The store must
/// be tracking changes.
pub fn publish(store: &mut Store, events: &broadcast::Sender<Arc<Event>>) -> Result<()> {
    let changes = store.take_changes();
    for kind in Kind::ALL {
        for event in store.events(kind, changes.ids(kind))? {
            // Having no subscribers right now is fine.
            let _ = events.send(Arc::new(event));
        }
    }
    Ok(())
}

pub fn format_counts(counts: &Counts) -> String {
    [
        ("observers", counts.observers),
        ("status reports", counts.statuses),
        ("packets", counts.packets),
        ("observations", counts.observations),
        ("undecrypted", counts.undecrypted),
        ("messages", counts.messages),
        ("adverts", counts.adverts),
        ("nodes", counts.nodes),
        ("channels", counts.channels),
    ]
    .iter()
    .map(|(label, count)| format!("{label:<15}{count:>10}"))
    .collect::<Vec<_>>()
    .join("\n")
}
