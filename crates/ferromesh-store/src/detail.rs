//! Node listings, single-packet detail and direct messages.

use ferromesh_model::{DirectMessageInfo, Event, Kind, NodeInfo, PacketDetail, PacketReception};
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

fn timestamp(row: &Row<'_>, index: usize) -> rusqlite::Result<Timestamp> {
    let micros: Micros = row.get(index)?;
    Timestamp::from_microsecond(micros)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(e)))
}
