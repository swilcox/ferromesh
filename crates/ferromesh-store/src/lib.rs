//! SQLite storage for ferromesh.
//!
//! Observer reports go in as [`Reception`]s. The store keeps one row per
//! packet (keyed by packet hash), one per observation, and whatever the packet
//! decodes to: adverts, nodes, channel messages. Derived rows depend only on
//! the set of receptions and channels, never on the order they arrive in, so a
//! database rebuilt from the raw log matches the live one ([`Store::digest`]).
//!
//! The [`Store`] is the single writer. Any number of read-only [`Reader`]s can
//! query alongside it.
//!
//! Timestamps are [`Micros`]: microseconds since the Unix epoch, UTC.

mod detail;
mod digest;
mod guess;
mod ingest;
mod read;
mod schema;
mod sql;

use std::path::Path;
use std::time::Duration;

use ferromesh_model::{
    Backfill, ChannelInfo, Event, Filter, GuessChannels, GuessReport, Kind, UnknownChannel,
};
use meshcore_proto::ChannelKey;
use rusqlite::{Connection, params};

pub use guess::BUILTIN_NAMES;
pub use ingest::{Batch, ObserverInfo, Outcome, Reception, StatusReport};
pub use read::{Order, Page, Reader};

/// Microseconds since the Unix epoch, UTC.
pub type Micros = i64;

/// Packets per transaction when decrypting stored traffic for a new channel.
const BACKFILL_BATCH: usize = 1000;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error("channel {id} has an invalid secret: {source}")]
    BadChannelSecret { id: i64, source: meshcore_proto::KeyError },

    #[error("channel {id} has unknown kind {kind:?}")]
    UnknownChannelKind { id: i64, kind: String },

    #[error("database schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: i64, supported: i64 },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// The built-in public channel.
    Public,
    /// Secret derived from the name.
    Hashtag,
    /// Secret supplied explicitly.
    Key,
}

impl ChannelKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Hashtag => "hashtag",
            Self::Key => "key",
        }
    }

    fn parse(kind: &str) -> Option<Self> {
        match kind {
            "public" => Some(Self::Public),
            "hashtag" => Some(Self::Hashtag),
            "key" => Some(Self::Key),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChannelRow {
    pub id: i64,
    pub name: String,
    pub key: ChannelKey,
    pub kind: ChannelKind,
    pub enabled: bool,
    pub added_at: Micros,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub observers: i64,
    pub statuses: i64,
    pub packets: i64,
    pub observations: i64,
    /// Channel packets no known key opens yet.
    pub undecrypted: i64,
    pub messages: i64,
    pub adverts: i64,
    pub nodes: i64,
    pub channels: i64,
}

/// The ids that committed writes created, per kind, in insertion order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    pub messages: Vec<i64>,
    pub packets: Vec<i64>,
    pub observations: Vec<i64>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.packets.is_empty() && self.observations.is_empty()
    }

    pub fn ids(&self, kind: Kind) -> &[i64] {
        match kind {
            Kind::Messages => &self.messages,
            Kind::Packets => &self.packets,
            Kind::Observations => &self.observations,
        }
    }

    fn append(&mut self, other: Self) {
        self.messages.extend(other.messages);
        self.packets.extend(other.packets);
        self.observations.extend(other.observations);
    }
}

pub struct Store {
    conn: Connection,
    /// Enabled channel keys by id, in the order decryption tries them.
    keys: Vec<(i64, ChannelKey)>,
    /// Rows created since the last `take_changes`, if tracking.
    changes: Option<Changes>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // The raw log is synced before every write, so a power cut that loses
        // the newest transactions is recoverable with a rebuild.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        schema::migrate(&mut conn)?;
        let mut store = Self { conn, keys: Vec::new(), changes: None };
        store.add_channel("public", &ChannelKey::public(), ChannelKind::Public, 0)?;
        store.load_keys()?;
        Ok(store)
    }

    /// Adds a channel unless one with the same secret exists. Returns whether
    /// it was added. Stored packets aren't touched until
    /// [`backfill_channel`](Self::backfill_channel).
    pub fn add_channel(
        &mut self,
        name: &str,
        key: &ChannelKey,
        kind: ChannelKind,
        added_at: Micros,
    ) -> Result<bool> {
        let added = self.conn.execute(
            "INSERT INTO channels (name, secret, hash, kind, added_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (secret) DO NOTHING",
            params![name, key.secret(), key.hash(), kind.as_str(), added_at],
        )? > 0;
        if added {
            self.load_keys()?;
        }
        Ok(added)
    }

    pub fn channels(&self) -> Result<Vec<ChannelRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, secret, kind, enabled, added_at FROM channels ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (id, name, secret, kind, enabled, added_at) = row?;
            Ok(ChannelRow {
                id,
                name,
                key: ChannelKey::from_secret(&secret)
                    .map_err(|source| Error::BadChannelSecret { id, source })?,
                kind: ChannelKind::parse(&kind).ok_or(Error::UnknownChannelKind { id, kind })?,
                enabled,
                added_at,
            })
        })
        .collect()
    }

    fn load_keys(&mut self) -> Result<()> {
        self.keys = self
            .channels()?
            .into_iter()
            .filter(|channel| channel.enabled)
            .map(|channel| (channel.id, channel.key))
            .collect();
        Ok(())
    }

    /// Decrypts stored packets that were waiting for a channel's key, one
    /// batch per transaction. The messages it creates show up in
    /// [`take_changes`](Self::take_changes) like any others.
    pub fn backfill_channel(&mut self, channel_id: i64) -> Result<Backfill> {
        let key = self.keys.iter().find(|(id, _)| *id == channel_id).map(|(_, key)| key.clone());
        let Some(key) = key else {
            return Ok(Backfill::default());
        };
        let mut total = Backfill::default();
        let mut after = 0;
        loop {
            let (progress, last) =
                self.write(|batch| batch.backfill(channel_id, &key, after, BACKFILL_BATCH))?;
            total.checked += progress.checked;
            total.decrypted += progress.decrypted;
            total.messages += progress.messages;
            match last {
                Some(last) => after = last,
                None => return Ok(total),
            }
        }
    }

    /// Records which rows each write creates, for [`take_changes`](Self::take_changes).
    /// Off by default, so imports and rebuilds don't pile up ids.
    pub fn track_changes(&mut self) {
        self.changes.get_or_insert_with(Changes::default);
    }

    /// The rows created since the last call.
    pub fn take_changes(&mut self) -> Changes {
        self.changes.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// Runs `f` in one transaction, committing only if it succeeds.
    pub fn write<T>(&mut self, f: impl FnOnce(&mut Batch<'_>) -> Result<T>) -> Result<T> {
        let mut batch = Batch::new(self.conn.transaction()?, &self.keys);
        let value = f(&mut batch)?;
        let changes = batch.commit()?;
        if let Some(pending) = &mut self.changes {
            pending.append(changes);
        }
        Ok(value)
    }

    pub fn max_id(&self, kind: Kind) -> Result<i64> {
        read::max_id(&self.conn, kind)
    }

    pub fn history(&self, kind: Kind, filter: &Filter, page: &Page) -> Result<Vec<Event>> {
        read::history(&self.conn, kind, filter, page)
    }

    pub fn events(&self, kind: Kind, ids: &[i64]) -> Result<Vec<Event>> {
        read::events(&self.conn, kind, ids)
    }

    pub fn channel_infos(&self) -> Result<Vec<ChannelInfo>> {
        read::channel_infos(&self.conn, None)
    }

    pub fn channel_info(&self, id: i64) -> Result<Option<ChannelInfo>> {
        Ok(read::channel_infos(&self.conn, Some(id))?.pop())
    }

    pub fn unknown_channels(&self) -> Result<Vec<UnknownChannel>> {
        read::unknown_channels(&self.conn)
    }

    pub fn guess_channels(&self, request: &GuessChannels) -> Result<GuessReport> {
        guess::guess_channels(&self.conn, request)
    }

    pub fn nodes(&self, limit: usize) -> Result<Vec<ferromesh_model::NodeInfo>> {
        detail::nodes(&self.conn, limit)
    }

    pub fn packet_detail(&self, hash: &[u8]) -> Result<Option<ferromesh_model::PacketDetail>> {
        detail::packet_detail(&self.conn, hash)
    }

    pub fn counts(&self) -> Result<Counts> {
        Ok(self.conn.query_row(
            "SELECT (SELECT count(*) FROM observers),
                    (SELECT count(*) FROM observer_status),
                    (SELECT count(*) FROM packets),
                    (SELECT count(*) FROM observations),
                    (SELECT count(*) FROM packets WHERE decode_state = 1),
                    (SELECT count(*) FROM messages),
                    (SELECT count(*) FROM adverts),
                    (SELECT count(*) FROM nodes),
                    (SELECT count(*) FROM channels)",
            [],
            |row| {
                Ok(Counts {
                    observers: row.get(0)?,
                    statuses: row.get(1)?,
                    packets: row.get(2)?,
                    observations: row.get(3)?,
                    undecrypted: row.get(4)?,
                    messages: row.get(5)?,
                    adverts: row.get(6)?,
                    nodes: row.get(7)?,
                    channels: row.get(8)?,
                })
            },
        )?)
    }

    /// A SHA-256 over every table's contents with row ids replaced by the
    /// natural keys they refer to. Equal digests mean equal databases,
    /// regardless of insertion order.
    pub fn digest(&self) -> Result<String> {
        Ok(digest::digest(&self.conn)?)
    }
}
