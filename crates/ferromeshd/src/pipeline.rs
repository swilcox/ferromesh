//! Raw records into the store, new rows out to stream subscribers, and
//! channels in. Shared by serve, import and rebuild.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use ferromesh_model::{ChannelAdded, ChannelInfo, Event, Kind};
use ferromesh_store::{ChannelKind, Counts, Outcome, Store};
use jiff::Timestamp;
use meshcore_proto::ChannelKey;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::config::Config;
use crate::rawlog::RawRecord;
use crate::source::{self, Message};

/// Opens the database, creating it if needed, and adds configured channels.
pub fn open_store(config: &Config) -> Result<Store> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating {}", config.data_dir.display()))?;
    let path = config.db_path();
    let mut store = Store::open(&path).with_context(|| format!("opening {}", path.display()))?;
    add_configured_channels(&mut store, config)?;
    Ok(store)
}

/// Adds channels from the config that the database lacks, decrypting stored
/// traffic for each, just as adding one through the API does.
pub fn add_configured_channels(store: &mut Store, config: &Config) -> Result<()> {
    for channel in &config.channels {
        let name = channel.name.trim();
        let (key, kind) = channel_key(name, channel.key.as_deref())?;
        if let AddOutcome::Added(added) = add_channel(store, name, &key, kind)? {
            info!(channel = %name, decrypted = added.backfill.decrypted, "added channel");
        }
    }
    Ok(())
}

/// A hashtag channel (`#name`) derives its key from the name; any other
/// channel needs its key, in hex or base64.
pub fn channel_key(name: &str, key: Option<&str>) -> Result<(ChannelKey, ChannelKind)> {
    ensure!(!name.is_empty(), "channel name is empty");
    match key {
        Some(key) => Ok((
            ChannelKey::parse(key).with_context(|| format!("channel {name:?}"))?,
            ChannelKind::Key,
        )),
        None if name.starts_with('#') && name.len() > 1 => {
            Ok((ChannelKey::from_hashtag(name), ChannelKind::Hashtag))
        }
        None => bail!("channel {name:?}: hashtag channels start with #; others need a key"),
    }
}

pub enum AddOutcome {
    Added(ChannelAdded),
    /// A channel with this key already exists.
    Exists(ChannelInfo),
}

/// Adds a channel and decrypts stored packets that were waiting for its key.
pub fn add_channel(
    store: &mut Store,
    name: &str,
    key: &ChannelKey,
    kind: ChannelKind,
) -> Result<AddOutcome> {
    let added = store.add_channel(name, key, kind, Timestamp::now().as_microsecond())?;
    let id = store
        .channels()?
        .into_iter()
        .find(|row| row.key == *key)
        .map(|row| row.id)
        .context("channel missing after adding it")?;
    if !added {
        return Ok(AddOutcome::Exists(store.channel_info(id)?.context("channel missing")?));
    }
    let backfill = store.backfill_channel(id)?;
    let channel = store.channel_info(id)?.context("channel missing after adding it")?;
    Ok(AddOutcome::Added(ChannelAdded { channel, backfill }))
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Tally {
    pub records: u64,
    pub observations: u64,
    pub new_packets: u64,
    pub duplicates: u64,
    pub statuses: u64,
    pub direct_messages: u64,
    pub sent: u64,
    pub acks: u64,
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
        self.direct_messages += other.direct_messages;
        self.sent += other.sent;
        self.acks += other.acks;
        self.malformed += other.malformed;
        self.ignored += other.ignored;
        self.unparsed += other.unparsed;
    }
}

impl fmt::Display for Tally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "records={} observations={} new_packets={} duplicates={} statuses={} \
             direct_messages={} sent={} acks={} malformed={} ignored={} unparsed={}",
            self.records,
            self.observations,
            self.new_packets,
            self.duplicates,
            self.statuses,
            self.direct_messages,
            self.sent,
            self.acks,
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
            match source::parse(record) {
                Err(e) => {
                    batch_tally.unparsed += 1;
                    warn!(topic = %record.topic, "unparseable message: {e:#}");
                }
                Ok(Message::Ignored) => batch_tally.ignored += 1,
                Ok(Message::Direct(message)) => {
                    if batch.record_direct_message(&message)? {
                        batch_tally.direct_messages += 1;
                    } else {
                        batch_tally.duplicates += 1;
                    }
                }
                Ok(Message::Sent(message)) => {
                    if batch.record_sent(&message)? {
                        batch_tally.sent += 1;
                    } else {
                        batch_tally.duplicates += 1;
                    }
                }
                Ok(Message::Ack(ack)) => {
                    if batch.record_ack(&ack)? {
                        batch_tally.acks += 1;
                    } else {
                        batch_tally.ignored += 1;
                    }
                }
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
        ("direct messages", counts.direct_messages),
        ("sent messages", counts.sent_messages),
    ]
    .iter()
    .map(|(label, count)| format!("{label:<15}{count:>10}"))
    .collect::<Vec<_>>()
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_keys() {
        let (key, kind) = channel_key("#test", None).unwrap();
        assert_eq!((key, kind), (ChannelKey::from_hashtag("#test"), ChannelKind::Hashtag));
        let (base64, kind) = channel_key("Family", Some("izOH6cXN6mrJ5e26oRXNcg==")).unwrap();
        assert_eq!(kind, ChannelKind::Key);
        let (hex, _) = channel_key("Family", Some("8b3387e9c5cdea6ac9e5edbaa115cd72")).unwrap();
        assert_eq!(hex, base64);
        assert!(channel_key("test", None).is_err());
        assert!(channel_key("#", None).is_err());
        assert!(channel_key("", None).is_err());
        assert!(channel_key("Family", Some("not base64")).is_err());
    }
}
