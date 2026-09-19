use rusqlite::Connection;
use rusqlite::types::ValueRef;
use sha2::{Digest, Sha256};

/// Every table as a canonical query: row ids are swapped for the natural keys
/// they point at (packet hash, observer and node pubkey, channel secret), and
/// rows are ordered by those keys. Wall-clock bookkeeping such as
/// `channels.added_at` is left out.
const CANONICAL: &[(&str, &str)] = &[
    (
        "observers",
        "SELECT pubkey, name, iata, first_seen_at, last_seen_at FROM observers ORDER BY pubkey",
    ),
    (
        "observer_status",
        "SELECT o.pubkey, s.at, s.status, s.model, s.firmware_version, s.radio, s.battery_mv,
                s.uptime_secs, s.noise_floor, s.tx_air_secs, s.rx_air_secs, s.packets_sent,
                s.packets_received, s.recv_errors, s.queue_len, s.raw
         FROM observer_status s JOIN observers o ON o.id = s.observer_id
         ORDER BY o.pubkey, s.at",
    ),
    ("channels", "SELECT secret, name, hash, kind, enabled FROM channels ORDER BY secret"),
    (
        "packets",
        "SELECT p.hash, p.payload_type, p.payload, p.first_seen_at, p.last_seen_at,
                p.observation_count, p.channel_hash, c.secret, p.decode_state
         FROM packets p LEFT JOIN channels c ON c.id = p.channel_id
         ORDER BY p.hash",
    ),
    (
        "observations",
        "SELECT o.pubkey, p.hash, x.rx_at, x.header, x.transport_codes, x.path_len, x.path,
                x.snr, x.rssi, x.score, x.direction
         FROM observations x
         JOIN observers o ON o.id = x.observer_id
         JOIN packets p ON p.id = x.packet_id
         ORDER BY o.pubkey, p.hash, x.rx_at",
    ),
    (
        "adverts",
        "SELECT p.hash, a.pubkey, a.adv_timestamp, a.flags, a.lat_e6, a.lon_e6, a.name,
                a.signature_ok
         FROM adverts a JOIN packets p ON p.id = a.packet_id
         ORDER BY p.hash",
    ),
    (
        "nodes",
        "SELECT n.pubkey, n.name, n.role, n.lat_e6, n.lon_e6, p.hash, n.adv_timestamp,
                n.first_seen_at, n.last_seen_at, n.advert_count
         FROM nodes n JOIN packets p ON p.id = n.advert_packet_id
         ORDER BY n.pubkey",
    ),
    (
        "node_names",
        "SELECT pubkey, name, first_seen_at, last_seen_at FROM node_names ORDER BY pubkey, name",
    ),
    (
        "messages",
        "SELECT p.hash, c.secret, m.first_seen_at, m.sender_timestamp, m.txt_type, m.attempt,
                m.sender, m.body
         FROM messages m
         JOIN packets p ON p.id = m.packet_id
         JOIN channels c ON c.id = m.channel_id
         ORDER BY p.hash",
    ),
    (
        "direct_messages",
        "SELECT o.pubkey, d.received_at, d.sender_prefix, d.path_len, d.txt_type,
                d.sender_timestamp, d.signer_prefix, d.snr, d.body
         FROM direct_messages d JOIN observers o ON o.id = d.observer_id
         ORDER BY o.pubkey, d.sender_prefix, d.sender_timestamp, d.txt_type, d.body",
    ),
    (
        "sent_messages",
        "SELECT o.pubkey, s.sent_at, s.channel, s.recipient, s.recipient_name, s.body,
                s.sender_timestamp, s.packet_hash, s.expected_ack, s.ack_timeout_ms, s.flood,
                s.error, s.acked_at, s.round_trip_ms
         FROM sent_messages s JOIN observers o ON o.id = s.observer_id
         ORDER BY o.pubkey, s.sender_timestamp, s.body",
    ),
];

pub(crate) fn digest(conn: &Connection) -> rusqlite::Result<String> {
    let mut sha = Sha256::new();
    for (table, sql) in CANONICAL {
        sha.update(table.as_bytes());
        sha.update([0]);
        let mut stmt = conn.prepare(sql)?;
        let columns = stmt.column_count();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            for column in 0..columns {
                encode(&mut sha, row.get_ref(column)?);
            }
            sha.update([b'\n']);
        }
    }
    Ok(hex::encode(sha.finalize()))
}

/// Type-tagged and length-prefixed, so different rows can't encode alike.
fn encode(sha: &mut Sha256, value: ValueRef<'_>) {
    match value {
        ValueRef::Null => sha.update([0]),
        ValueRef::Integer(n) => {
            sha.update([1]);
            sha.update(n.to_le_bytes());
        }
        ValueRef::Real(r) => {
            sha.update([2]);
            sha.update(r.to_bits().to_le_bytes());
        }
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            sha.update([if matches!(value, ValueRef::Text(_)) { 3 } else { 4 }]);
            sha.update((bytes.len() as u64).to_le_bytes());
            sha.update(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CANONICAL;
    use crate::Store;

    #[test]
    fn digest_covers_every_table() {
        let store = Store::open_in_memory().unwrap();
        let mut stmt = store
            .conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'messages_fts%'
                 ORDER BY name",
            )
            .unwrap();
        let tables: Vec<String> =
            stmt.query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        let mut covered: Vec<&str> = CANONICAL.iter().map(|(table, _)| *table).collect();
        covered.sort_unstable();
        assert_eq!(tables, covered, "add new tables to CANONICAL");
    }
}
