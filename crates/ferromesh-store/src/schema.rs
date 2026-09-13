use rusqlite::Connection;

use crate::{Error, Result};

/// Each entry moves the schema up one version; `PRAGMA user_version` records
/// how many have run. Append new migrations, never edit old ones.
const MIGRATIONS: &[&str] = &[r#"
-- Timestamps (*_at) are INTEGER microseconds since the Unix epoch, UTC.

CREATE TABLE observers (
    id            INTEGER PRIMARY KEY,
    pubkey        BLOB NOT NULL UNIQUE,
    name          TEXT,
    iata          TEXT,
    first_seen_at INTEGER NOT NULL,
    last_seen_at  INTEGER NOT NULL
);

CREATE TABLE observer_status (
    observer_id      INTEGER NOT NULL REFERENCES observers (id),
    at               INTEGER NOT NULL,
    status           TEXT,
    model            TEXT,
    firmware_version TEXT,
    radio            TEXT,
    battery_mv       INTEGER,
    uptime_secs      INTEGER,
    noise_floor      INTEGER,
    tx_air_secs      INTEGER,
    rx_air_secs      INTEGER,
    packets_sent     INTEGER,
    packets_received INTEGER,
    recv_errors      INTEGER,
    queue_len        INTEGER,
    raw              TEXT NOT NULL,
    PRIMARY KEY (observer_id, at)
) WITHOUT ROWID;

CREATE TABLE channels (
    id       INTEGER PRIMARY KEY,
    name     TEXT NOT NULL,
    secret   BLOB NOT NULL UNIQUE,
    hash     INTEGER NOT NULL,
    kind     TEXT NOT NULL,
    enabled  INTEGER NOT NULL DEFAULT 1,
    added_at INTEGER NOT NULL
);

-- decode_state: 0 cleartext, 1 undecrypted channel packet, 2 decrypted,
-- 3 sealed (needs an endpoint's key), 4 malformed payload.
CREATE TABLE packets (
    id                INTEGER PRIMARY KEY,
    hash              BLOB NOT NULL UNIQUE,
    payload_type      INTEGER NOT NULL,
    payload           BLOB NOT NULL,
    first_seen_at     INTEGER NOT NULL,
    last_seen_at      INTEGER NOT NULL,
    observation_count INTEGER NOT NULL DEFAULT 0,
    channel_hash      INTEGER,
    channel_id        INTEGER REFERENCES channels (id),
    decode_state      INTEGER NOT NULL
);
CREATE INDEX packets_by_time ON packets (first_seen_at);
CREATE INDEX packets_by_type ON packets (payload_type, first_seen_at);
CREATE INDEX packets_undecrypted ON packets (channel_hash) WHERE decode_state = 1;

-- header, transport_codes, path_len and path plus the packet's payload
-- reassemble the frame exactly as this observer heard it.
CREATE TABLE observations (
    id              INTEGER PRIMARY KEY,
    packet_id       INTEGER NOT NULL REFERENCES packets (id),
    observer_id     INTEGER NOT NULL REFERENCES observers (id),
    rx_at           INTEGER NOT NULL,
    header          INTEGER NOT NULL,
    transport_codes BLOB,
    path_len        INTEGER NOT NULL,
    path            BLOB NOT NULL,
    snr             REAL,
    rssi            INTEGER,
    score           INTEGER,
    direction       TEXT NOT NULL DEFAULT 'rx',
    UNIQUE (observer_id, packet_id, rx_at)
);
CREATE INDEX observations_by_time ON observations (rx_at);
CREATE INDEX observations_by_packet ON observations (packet_id);

CREATE TABLE adverts (
    packet_id     INTEGER PRIMARY KEY REFERENCES packets (id),
    pubkey        BLOB NOT NULL,
    adv_timestamp INTEGER NOT NULL,
    flags         INTEGER,
    lat_e6        INTEGER,
    lon_e6        INTEGER,
    name          TEXT,
    signature_ok  INTEGER NOT NULL
);
CREATE INDEX adverts_by_node ON adverts (pubkey, adv_timestamp);

-- Fields come from the node's newest validly signed advert.
CREATE TABLE nodes (
    pubkey           BLOB PRIMARY KEY,
    name             TEXT,
    role             INTEGER,
    lat_e6           INTEGER,
    lon_e6           INTEGER,
    advert_packet_id INTEGER NOT NULL REFERENCES packets (id),
    adv_timestamp    INTEGER NOT NULL,
    first_seen_at    INTEGER NOT NULL,
    last_seen_at     INTEGER NOT NULL,
    advert_count     INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE node_names (
    pubkey        BLOB NOT NULL,
    name          TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at  INTEGER NOT NULL,
    PRIMARY KEY (pubkey, name)
) WITHOUT ROWID;
CREATE INDEX node_names_by_name ON node_names (name);

-- sender is the unauthenticated display name typed into the sending radio.
CREATE TABLE messages (
    id               INTEGER PRIMARY KEY,
    packet_id        INTEGER NOT NULL UNIQUE REFERENCES packets (id),
    channel_id       INTEGER NOT NULL REFERENCES channels (id),
    first_seen_at    INTEGER NOT NULL,
    sender_timestamp INTEGER NOT NULL,
    txt_type         INTEGER NOT NULL,
    attempt          INTEGER NOT NULL,
    sender           TEXT,
    body             TEXT NOT NULL
);
CREATE INDEX messages_by_channel ON messages (channel_id, first_seen_at);
CREATE INDEX messages_by_sender ON messages (sender, first_seen_at);
CREATE INDEX messages_by_time ON messages (first_seen_at);

CREATE VIRTUAL TABLE messages_fts USING fts5 (
    sender, body, content = 'messages', content_rowid = 'id'
);
CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, sender, body) VALUES (new.id, new.sender, new.body);
END;
CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, sender, body)
    VALUES ('delete', old.id, old.sender, old.body);
END;
CREATE TRIGGER messages_fts_update AFTER UPDATE OF sender, body ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, sender, body)
    VALUES ('delete', old.id, old.sender, old.body);
    INSERT INTO messages_fts (rowid, sender, body) VALUES (new.id, new.sender, new.body);
END;
"#];

pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let supported = MIGRATIONS.len() as i64;
    if found > supported {
        return Err(Error::SchemaTooNew { found, supported });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}
