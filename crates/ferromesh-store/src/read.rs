//! Queries shared by the read-only [`Reader`] and the writing
//! [`Store`](crate::Store): history pages, events by id, and the newest id.

use std::path::Path;
use std::time::Duration;

use ferromesh_model::{
    Advert, DecodeState, Event, Filter, Kind, MessageEvent, ObservationEvent, PacketEvent,
};
use jiff::Timestamp;
use meshcore_proto::{Header, NodeRole, PayloadType};
use rusqlite::types::Type;
use rusqlite::{Connection, OpenFlags, Row, params_from_iter};

use crate::{Micros, Result, sql};

/// A read-only connection; any number can run alongside the writer.
pub struct Reader {
    conn: Connection,
}

impl Reader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { conn })
    }

    pub fn max_id(&self, kind: Kind) -> Result<i64> {
        max_id(&self.conn, kind)
    }

    pub fn history(&self, kind: Kind, filter: &Filter, page: &Page) -> Result<Vec<Event>> {
        history(&self.conn, kind, filter, page)
    }

    pub fn events(&self, kind: Kind, ids: &[i64]) -> Result<Vec<Event>> {
        events(&self.conn, kind, ids)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    Ascending,
    #[default]
    Descending,
}

/// A slice of history. Id bounds are exclusive; time bounds are inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub since: Option<Micros>,
    pub until: Option<Micros>,
    pub limit: usize,
    pub order: Order,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            after: None,
            before: None,
            since: None,
            until: None,
            limit: 100,
            order: Order::Descending,
        }
    }
}

/// How to select and decode one kind. Column aliases here are the ones
/// `sql::conditions` refers to.
struct Shape {
    table: &'static str,
    select: &'static str,
    id: &'static str,
    time: &'static str,
    decode: fn(&Row<'_>) -> rusqlite::Result<Event>,
}

const MESSAGES: Shape = Shape {
    table: "messages",
    select: "SELECT m.id, p.hash, m.first_seen_at, c.name, m.sender, m.body, m.sender_timestamp,
                    m.txt_type, m.attempt, p.observation_count
             FROM messages m
             JOIN packets p ON p.id = m.packet_id
             JOIN channels c ON c.id = m.channel_id",
    id: "m.id",
    time: "m.first_seen_at",
    decode: message,
};

const PACKETS: Shape = Shape {
    table: "packets",
    select: "SELECT p.id, p.hash, p.payload_type, p.first_seen_at, p.last_seen_at,
                    p.observation_count, p.decode_state, length(p.payload), c.name, p.channel_hash,
                    a.pubkey, a.name, a.flags, a.lat_e6, a.lon_e6, a.signature_ok, m.sender, m.body
             FROM packets p
             LEFT JOIN channels c ON c.id = p.channel_id
             LEFT JOIN adverts a ON a.packet_id = p.id
             LEFT JOIN messages m ON m.packet_id = p.id",
    id: "p.id",
    time: "p.first_seen_at",
    decode: packet,
};

const OBSERVATIONS: Shape = Shape {
    table: "observations",
    select: "SELECT o.id, o.packet_id, p.hash, p.payload_type, o.rx_at,
                    coalesce(ob.name, hex(ob.pubkey)), o.header, o.path_len, o.path, o.snr, o.rssi,
                    c.name, a.pubkey, a.name, m.sender, m.body
             FROM observations o
             JOIN packets p ON p.id = o.packet_id
             JOIN observers ob ON ob.id = o.observer_id
             LEFT JOIN channels c ON c.id = p.channel_id
             LEFT JOIN adverts a ON a.packet_id = p.id
             LEFT JOIN messages m ON m.packet_id = p.id",
    id: "o.id",
    time: "o.rx_at",
    decode: observation,
};

const fn shape(kind: Kind) -> Shape {
    match kind {
        Kind::Messages => MESSAGES,
        Kind::Packets => PACKETS,
        Kind::Observations => OBSERVATIONS,
    }
}

/// The newest id of `kind`, or 0 if there are none.
pub(crate) fn max_id(conn: &Connection, kind: Kind) -> Result<i64> {
    let sql = format!("SELECT coalesce(max(id), 0) FROM {}", shape(kind).table);
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

pub(crate) fn history(
    conn: &Connection,
    kind: Kind,
    filter: &Filter,
    page: &Page,
) -> Result<Vec<Event>> {
    let shape = shape(kind);
    let mut conditions = sql::conditions(kind, filter);
    if let Some(after) = page.after {
        conditions.push(format!("{} > ?", shape.id), [after.into()]);
    }
    if let Some(before) = page.before {
        conditions.push(format!("{} < ?", shape.id), [before.into()]);
    }
    if let Some(since) = page.since {
        conditions.push(format!("{} >= ?", shape.time), [since.into()]);
    }
    if let Some(until) = page.until {
        conditions.push(format!("{} <= ?", shape.time), [until.into()]);
    }
    let order = match page.order {
        Order::Ascending => "ASC",
        Order::Descending => "DESC",
    };
    let sql = format!(
        "{} WHERE {} ORDER BY {} {order} LIMIT {}",
        shape.select,
        conditions.sql(),
        shape.id,
        page.limit
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let events = stmt
        .query_map(params_from_iter(conditions.params()), shape.decode)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(events)
}

/// Events by id, oldest first. Unknown ids are skipped.
pub(crate) fn events(conn: &Connection, kind: Kind, ids: &[i64]) -> Result<Vec<Event>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let shape = shape(kind);
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql =
        format!("{} WHERE {} IN ({placeholders}) ORDER BY {}", shape.select, shape.id, shape.id);
    let mut stmt = conn.prepare(&sql)?;
    let events =
        stmt.query_map(params_from_iter(ids), shape.decode)?.collect::<rusqlite::Result<_>>()?;
    Ok(events)
}

fn message(row: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event::Message(MessageEvent {
        id: row.get(0)?,
        packet_hash: hash(row, 1)?,
        first_seen_at: timestamp(row, 2)?,
        channel: row.get(3)?,
        sender: row.get(4)?,
        body: row.get(5)?,
        sender_timestamp: row.get(6)?,
        txt_type: row.get(7)?,
        attempt: row.get(8)?,
        heard: row.get(9)?,
    }))
}

fn packet(row: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event::Packet(PacketEvent {
        id: row.get(0)?,
        hash: hash(row, 1)?,
        payload_type: payload_type(row, 2)?,
        first_seen_at: timestamp(row, 3)?,
        last_seen_at: timestamp(row, 4)?,
        heard: row.get(5)?,
        decode_state: decode_state(row, 6)?,
        size: row.get(7)?,
        channel: row.get(8)?,
        channel_hash: row.get(9)?,
        advert: advert(row, 10)?,
        text: text(row, 16)?,
    }))
}

fn observation(row: &Row<'_>) -> rusqlite::Result<Event> {
    let header: u8 = row.get(6)?;
    let path_len: u8 = row.get(7)?;
    let path: Vec<u8> = row.get(8)?;
    let hash_size = usize::from(path_len >> 6) + 1;
    Ok(Event::Observation(ObservationEvent {
        id: row.get(0)?,
        packet_id: row.get(1)?,
        hash: hash(row, 2)?,
        payload_type: payload_type(row, 3)?,
        rx_at: timestamp(row, 4)?,
        observer: row.get(5)?,
        route: Header(header).route_type().name().to_owned(),
        hops: path.chunks(hash_size).map(hex::encode).collect(),
        snr: row.get(9)?,
        rssi: row.get(10)?,
        channel: row.get(11)?,
        advert_pubkey: row.get::<_, Option<Vec<u8>>>(12)?.map(hex::encode),
        advert_name: row.get(13)?,
        text: text(row, 14)?,
    }))
}

/// The six advert columns starting at `start`, if the packet is an advert.
fn advert(row: &Row<'_>, start: usize) -> rusqlite::Result<Option<Advert>> {
    let Some(pubkey) = row.get::<_, Option<Vec<u8>>>(start)? else {
        return Ok(None);
    };
    let flags: Option<u8> = row.get(start + 2)?;
    let degrees = |e6: Option<i32>| e6.map(|value| f64::from(value) / 1e6);
    Ok(Some(Advert {
        pubkey: hex::encode(pubkey),
        name: row.get(start + 1)?,
        role: flags.map(|flags| NodeRole::from_flags(flags).name().to_owned()),
        lat: degrees(row.get(start + 3)?),
        lon: degrees(row.get(start + 4)?),
        signature_ok: row.get(start + 5)?,
    }))
}

/// `"Sender: body"` from the sender and body columns starting at `start`.
fn text(row: &Row<'_>, start: usize) -> rusqlite::Result<Option<String>> {
    let sender: Option<String> = row.get(start)?;
    let body: Option<String> = row.get(start + 1)?;
    Ok(body.map(|body| match sender {
        Some(sender) => format!("{sender}: {body}"),
        None => body,
    }))
}

fn hash(row: &Row<'_>, index: usize) -> rusqlite::Result<String> {
    Ok(hex::encode_upper(row.get::<_, Vec<u8>>(index)?))
}

fn payload_type(row: &Row<'_>, index: usize) -> rusqlite::Result<String> {
    Ok(PayloadType::from_nibble(row.get(index)?).name().to_owned())
}

fn decode_state(row: &Row<'_>, index: usize) -> rusqlite::Result<DecodeState> {
    let code: i64 = row.get(index)?;
    DecodeState::from_code(code).ok_or(rusqlite::Error::IntegralValueOutOfRange(index, code))
}

fn timestamp(row: &Row<'_>, index: usize) -> rusqlite::Result<Timestamp> {
    Timestamp::from_microsecond(row.get(index)?)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(e)))
}
