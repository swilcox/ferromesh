//! Node listings, single-packet detail, direct messages and the outbox.

use ferromesh_model::{
    DirectMessageInfo, Event, Kind, NodeInfo, PacketDetail, PacketReception, SendStatus,
    SentMessageInfo,
};
use jiff::Timestamp;
use meshcore_proto::NodeRole;
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row};

use crate::{Micros, Result, read};

/// Nodes, most recently heard first.
pub(crate) fn nodes(conn: &Connection, limit: usize) -> Result<Vec<NodeInfo>> {
    let mut stmt = conn.prepare_cached(
        "SELECT pubkey, name, role, lat_e6, lon_e6, first_seen_at, last_seen_at, advert_count
         FROM nodes
         ORDER BY last_seen_at DESC, pubkey
         LIMIT ?1",
    )?;
    let degrees = |e6: Option<i32>| e6.map(|value| f64::from(value) / 1e6);
    let nodes = stmt
        .query_map([limit as i64], |row| {
            let role: Option<u8> = row.get(2)?;
            Ok(NodeInfo {
                pubkey: hex::encode(row.get::<_, Vec<u8>>(0)?),
                name: row.get(1)?,
                role: role.map(|role| NodeRole::from_flags(role).name().to_owned()),
                lat: degrees(row.get(3)?),
                lon: degrees(row.get(4)?),
                first_seen_at: timestamp(row, 5)?,
                last_seen_at: timestamp(row, 6)?,
                adverts: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(nodes)
}

/// A packet by hash, with each reception's frame put back together from the
/// stored header, transport codes, path and payload.
pub(crate) fn packet_detail(conn: &Connection, hash: &[u8]) -> Result<Option<PacketDetail>> {
    let found: Option<(i64, Vec<u8>)> = conn
        .prepare_cached("SELECT id, payload FROM packets WHERE hash = ?1")?
        .query_row([hash], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let Some((packet_id, payload)) = found else {
        return Ok(None);
    };
    let Some(Event::Packet(packet)) = read::events(conn, Kind::Packets, &[packet_id])?.pop() else {
        return Ok(None);
    };

    let mut stmt = conn.prepare_cached(
        "SELECT o.id, coalesce(ob.name, hex(ob.pubkey)), o.rx_at, o.snr, o.rssi, o.header,
                o.transport_codes, o.path_len, o.path
         FROM observations o
         JOIN observers ob ON ob.id = o.observer_id
         WHERE o.packet_id = ?1
         ORDER BY o.rx_at, o.id",
    )?;
    let receptions = stmt
        .query_map([packet_id], |row| {
            let mut frame = vec![row.get::<_, u8>(5)?];
            frame.extend(row.get::<_, Option<Vec<u8>>>(6)?.unwrap_or_default());
            frame.push(row.get(7)?);
            frame.extend(row.get::<_, Vec<u8>>(8)?);
            frame.extend_from_slice(&payload);
            Ok(PacketReception {
                observation_id: row.get(0)?,
                observer: row.get(1)?,
                rx_at: timestamp(row, 2)?,
                snr: row.get(3)?,
                rssi: row.get(4)?,
                frame: hex::encode_upper(frame),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(PacketDetail { packet, receptions }))
}

/// Direct messages, newest first. A sender is named only when exactly one
/// known node has its key prefix.
pub(crate) fn direct_messages(conn: &Connection, limit: usize) -> Result<Vec<DirectMessageInfo>> {
    let mut stmt = conn.prepare_cached(
        "SELECT d.id, d.received_at, coalesce(o.name, lower(hex(o.pubkey))), d.sender_prefix,
                (SELECT CASE WHEN count(*) = 1 THEN max(n.name) END
                 FROM nodes n WHERE substr(n.pubkey, 1, 6) = d.sender_prefix),
                d.path_len, d.txt_type, d.sender_timestamp, d.snr, d.body
         FROM direct_messages d JOIN observers o ON o.id = d.observer_id
         ORDER BY d.received_at DESC, d.id DESC
         LIMIT ?1",
    )?;
    let messages = stmt
        .query_map([limit as i64], |row| {
            let sent: i64 = row.get(7)?;
            Ok(DirectMessageInfo {
                id: row.get(0)?,
                received_at: timestamp(row, 1)?,
                to: row.get(2)?,
                sender: row.get(4)?,
                sender_prefix: hex::encode(row.get::<_, Vec<u8>>(3)?),
                hops: row.get(5)?,
                txt_type: row.get(6)?,
                sender_timestamp: Timestamp::from_second(sent).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(7, Type::Integer, Box::new(e))
                })?,
                snr: row.get(8)?,
                body: row.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(messages)
}

/// What a send's `to` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendTarget {
    Channel {
        name: String,
        secret: Vec<u8>,
    },
    Node(NodeContact),
    /// Several nodes match; each described as `name (key prefix)`.
    Ambiguous(Vec<String>),
    Unknown,
}

/// A node as its newest signed advert describes it: what a companion radio
/// needs to add it as a contact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeContact {
    pub pubkey: [u8; 32],
    pub name: Option<String>,
    /// Advert flags' low nibble: 1 chat, 2 repeater, 3 room server, 4 sensor.
    pub role: u8,
    pub adv_timestamp: u32,
    pub lat_e6: Option<i32>,
    pub lon_e6: Option<i32>,
}

/// Resolves `to`: a known channel by name (any case), else a node by its
/// exact advertised name (any case), else a node by a hex prefix of its key.
pub(crate) fn send_target(conn: &Connection, to: &str) -> Result<SendTarget> {
    let to = to.trim();
    let channel = conn
        .prepare_cached(
            "SELECT name, secret FROM channels WHERE lower(name) = lower(?1) AND enabled",
        )?
        .query_row([to], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    if let Some((name, secret)) = channel {
        return Ok(SendTarget::Channel { name, secret });
    }

    let select = "SELECT pubkey, name, role, adv_timestamp, lat_e6, lon_e6 FROM nodes";
    let mut nodes = node_contacts(conn, &format!("{select} WHERE lower(name) = lower(?1)"), [to])?;
    if nodes.is_empty() && to.len() >= 2 && to.bytes().all(|b| b.is_ascii_hexdigit()) {
        nodes = node_contacts(
            conn,
            &format!("{select} WHERE instr(lower(hex(pubkey)), lower(?1)) = 1"),
            [to],
        )?;
    }
    Ok(match nodes.len() {
        0 => SendTarget::Unknown,
        1 => SendTarget::Node(nodes.remove(0)),
        _ => SendTarget::Ambiguous(
            nodes
                .iter()
                .map(|node| {
                    let name = node.name.as_deref().unwrap_or("(unnamed)");
                    format!("{name} ({})", hex::encode(&node.pubkey[..4]))
                })
                .collect(),
        ),
    })
}

/// Nodes whose key starts with `prefix`, most recently heard first.
pub(crate) fn nodes_by_key_prefix(conn: &Connection, prefix: &[u8]) -> Result<Vec<NodeContact>> {
    let sql = "SELECT pubkey, name, role, adv_timestamp, lat_e6, lon_e6 FROM nodes
               WHERE substr(pubkey, 1, length(?1)) = ?1";
    node_contacts(conn, sql, [prefix])
}

fn node_contacts(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<NodeContact>> {
    let mut stmt = conn.prepare_cached(&format!("{sql} ORDER BY last_seen_at DESC LIMIT 10"))?;
    let nodes = stmt
        .query_map(params, |row| {
            let pubkey: Vec<u8> = row.get(0)?;
            Ok(NodeContact {
                pubkey: pubkey.try_into().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        Type::Blob,
                        "not a 32-byte key".into(),
                    )
                })?,
                name: row.get(1)?,
                role: row.get::<_, Option<u8>>(2)?.unwrap_or(0) & 0x0F,
                adv_timestamp: row.get(3)?,
                lat_e6: row.get(4)?,
                lon_e6: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(nodes)
}

/// How long past the radio's own estimate to wait for an acknowledgement
/// before calling it overdue.
const ACK_GRACE_MICROS: Micros = 5_000_000;

/// Sent messages, newest first, with status as of `now`.
pub(crate) fn outbox(conn: &Connection, limit: usize, now: Micros) -> Result<Vec<SentMessageInfo>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.id, s.sent_at, coalesce(o.name, lower(hex(o.pubkey))), s.channel,
                coalesce(s.recipient_name, lower(hex(substr(s.recipient, 1, 6)))), s.body,
                s.packet_hash, s.ack_timeout_ms, s.error, s.acked_at, s.round_trip_ms,
                (SELECT count(*) FROM packets p JOIN observations x ON x.packet_id = p.id
                 WHERE p.hash = s.packet_hash),
                (SELECT group_concat(DISTINCT coalesce(b.name, lower(hex(b.pubkey))))
                 FROM packets p JOIN observations x ON x.packet_id = p.id
                 JOIN observers b ON b.id = x.observer_id
                 WHERE p.hash = s.packet_hash),
                s.sender_timestamp
         FROM sent_messages s JOIN observers o ON o.id = s.observer_id
         ORDER BY s.sent_at DESC, s.id DESC
         LIMIT ?1",
    )?;
    let messages = stmt
        .query_map([limit as i64], |row| {
            let sent_at: Micros = row.get(1)?;
            let channel: Option<String> = row.get(3)?;
            let recipient: Option<String> = row.get(4)?;
            let packet_hash: Option<Vec<u8>> = row.get(6)?;
            let timeout_ms: Option<i64> = row.get(7)?;
            let error: Option<String> = row.get(8)?;
            let acked_at: Option<Micros> = row.get(9)?;
            let heard: i64 = row.get(11)?;
            let heard_by: Option<String> = row.get(12)?;
            let overdue = timeout_ms.is_some_and(|ms| now > sent_at + ms * 1000 + ACK_GRACE_MICROS);
            let status = match (&error, &channel) {
                (Some(_), _) => SendStatus::Failed,
                (None, Some(_)) if heard > 0 => SendStatus::Heard,
                (None, Some(_)) => SendStatus::Sent,
                (None, None) if acked_at.is_some() => SendStatus::Delivered,
                (None, None) if overdue => SendStatus::Unacknowledged,
                (None, None) => SendStatus::Sent,
            };
            Ok(SentMessageInfo {
                id: row.get(0)?,
                sent_at: timestamp(row, 1)?,
                from: row.get(2)?,
                direct: channel.is_none(),
                to: channel.or(recipient).unwrap_or_default(),
                body: row.get(5)?,
                sender_timestamp: Timestamp::from_second(row.get(13)?).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(13, Type::Integer, Box::new(e))
                })?,
                status,
                error,
                round_trip_ms: row.get(10)?,
                heard,
                heard_by: heard_by
                    .map(|names| names.split(',').map(str::to_owned).collect())
                    .unwrap_or_default(),
                packet_hash: packet_hash.map(hex::encode_upper),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(messages)
}

fn timestamp(row: &Row<'_>, index: usize) -> rusqlite::Result<Timestamp> {
    let micros: Micros = row.get(index)?;
    Timestamp::from_microsecond(micros)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(e)))
}
