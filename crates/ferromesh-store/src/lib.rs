//! SQLite storage for ferromesh.
//!
//! Observer reports go in as [`Reception`]s. The store keeps one row per
//! packet (keyed by packet hash), one per observation, and whatever the packet
//! decodes to: adverts, nodes, channel messages. Derived rows depend only on
//! the set of receptions, never on the order they arrive in, so a database
//! rebuilt from the raw log matches the live one ([`Store::digest`]).
//!
//! Timestamps are [`Micros`]: microseconds since the Unix epoch, UTC.

mod digest;
mod ingest;
mod schema;

use std::path::Path;
use std::time::Duration;

use meshcore_proto::ChannelKey;
use rusqlite::{Connection, params};

pub use ingest::{Batch, ObserverInfo, Outcome, Reception, StatusReport};

/// Microseconds since the Unix epoch, UTC.
pub type Micros = i64;

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

pub struct Store {
    conn: Connection,
    /// Enabled channel keys by id, in the order decryption tries them.
    keys: Vec<(i64, ChannelKey)>,
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
        let mut store = Self { conn, keys: Vec::new() };
        store.add_channel("public", &ChannelKey::public(), ChannelKind::Public, 0)?;
        store.load_keys()?;
        Ok(store)
    }

    /// Adds a channel unless one with the same secret exists. Returns whether
    /// it was added.
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

    /// Runs `f` in one transaction, committing only if it succeeds.
    pub fn write<T>(&mut self, f: impl FnOnce(&mut Batch<'_>) -> Result<T>) -> Result<T> {
        let mut batch = Batch::new(self.conn.transaction()?, &self.keys);
        let value = f(&mut batch)?;
        batch.commit()?;
        Ok(value)
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
