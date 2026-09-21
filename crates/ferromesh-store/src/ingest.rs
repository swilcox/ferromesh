//! Turning receptions and status reports into rows.
//!
//! Every derived value is order-independent: seen windows use min/max, counts
//! only grow on first sight, and node fields come from the newest signed
//! advert by (advert timestamp, packet hash). Replaying the same receptions in
//! any order therefore produces the same database, and so does adding a
//! channel later and backfilling it.

use ferromesh_model::Backfill;
use meshcore_proto::{
    Advert, ChannelKey, GroupPayload, GroupText, Packet, PacketHash, Payload, PayloadType,
    split_sender,
};
use rusqlite::{OptionalExtension, Transaction, params};

use crate::{Changes, Micros, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverInfo {
    pub pubkey: [u8; 32],
    pub name: Option<String>,
    /// Region code from the topic, e.g. `BNA`.
    pub iata: Option<String>,
    pub kind: ObserverKind,
}

/// How ferromesh hears from an observer, which decides what to expect of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ObserverKind {
    /// It publishes what it hears to a broker.
    #[default]
    Mqtt,
    /// Our own radio, on a serial port. Its firmware never hands over a
    /// packet larger than [`COMPANION_MAX_FRAME`] bytes.
    Companion,
}

/// The largest packet a companion radio reports hearing. Its firmware skips
/// anything longer, counting it but never logging it, so a companion's
/// delivery share can't reach 100% on a mesh that carries bigger packets.
pub const COMPANION_MAX_FRAME: usize = 173;

impl ObserverKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mqtt => "mqtt",
            Self::Companion => "companion",
        }
    }

    /// Anything unrecognised, including an observer recorded before this
    /// column existed, is taken to publish over MQTT.
    pub fn parse(text: &str) -> Self {
        match text {
            "companion" => Self::Companion,
            _ => Self::Mqtt,
        }
    }
}

/// One observer hearing one packet.
#[derive(Debug, Clone, PartialEq)]
pub struct Reception {
    pub observer: ObserverInfo,
    /// The observer's receive time.
    pub rx_at: Micros,
    /// The packet exactly as received over the air.
    pub frame: Vec<u8>,
    pub snr: Option<f64>,
    pub rssi: Option<i64>,
    pub score: Option<i64>,
    /// `rx` unless the observer reports otherwise.
    pub direction: Option<String>,
}

/// An observer's periodic health report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub observer: ObserverInfo,
    pub at: Micros,
    pub status: Option<String>,
    pub model: Option<String>,
    pub firmware_version: Option<String>,
    pub radio: Option<String>,
    pub battery_mv: Option<i64>,
    pub uptime_secs: Option<i64>,
    pub noise_floor: Option<i64>,
    pub tx_air_secs: Option<i64>,
    pub rx_air_secs: Option<i64>,
    pub packets_sent: Option<i64>,
    pub packets_received: Option<i64>,
    pub recv_errors: Option<i64>,
    pub queue_len: Option<i64>,
    /// The report as received, for fields not broken out.
    pub raw: String,
}

/// A direct message a companion radio decrypted and handed over.
#[derive(Debug, Clone, PartialEq)]
pub struct DirectMessage {
    /// The companion radio it was sent to.
    pub observer: ObserverInfo,
    /// When ferromesh fetched it from the radio.
    pub received_at: Micros,
    /// The first 6 bytes of the sender's key.
    pub sender_prefix: [u8; 6],
    /// Hops it travelled, or `None` if it came by a direct route.
    pub path_len: Option<u8>,
    pub txt_type: u8,
    pub sender_timestamp: u32,
    /// For signed room-server posts, the author's 4-byte key prefix.
    pub signer_prefix: Option<[u8; 4]>,
    pub snr: Option<f64>,
    pub body: String,
}

/// A message sent through a companion radio: to a channel or to one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    /// The companion radio that sent it.
    pub observer: ObserverInfo,
    pub sent_at: Micros,
    pub to: SentTo,
    pub body: String,
    /// The timestamp inside the message, which makes each send unique.
    pub sender_timestamp: u32,
    /// Why the radio refused it; `None` if it was transmitted.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentTo {
    Channel {
        name: String,
        /// The hash the packet has on the air.
        packet_hash: [u8; 8],
    },
    Node {
        pubkey: [u8; 32],
        name: Option<String>,
        /// The acknowledgement code to expect, once the radio has sent it.
        expected_ack: Option<u32>,
        ack_timeout_ms: Option<u32>,
        flood: Option<bool>,
    },
}

/// A recipient's acknowledgement of a direct message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acknowledgement {
    pub observer: ObserverInfo,
    pub at: Micros,
    pub ack: u32,
    pub round_trip_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A new observation; `new_packet` is false if another copy was stored first.
    Recorded { packet_id: i64, new_packet: bool },
    /// This exact observation was already stored.
    Duplicate,
    /// The frame didn't parse, so there is no hash to store it under.
    Malformed(meshcore_proto::Error),
}

#[derive(Clone, Copy)]
enum DecodeState {
    Cleartext = 0,
    Undecrypted = 1,
    Decrypted = 2,
    Sealed = 3,
    Malformed = 4,
}

/// Writes within one transaction; see [`crate::Store::write`].
pub struct Batch<'a> {
    tx: Transaction<'a>,
    keys: &'a [(i64, ChannelKey)],
    changes: Changes,
}

impl<'a> Batch<'a> {
    pub(crate) fn new(tx: Transaction<'a>, keys: &'a [(i64, ChannelKey)]) -> Self {
        Self { tx, keys, changes: Changes::default() }
    }

    /// Commits, returning the rows this batch created.
    pub(crate) fn commit(self) -> Result<Changes> {
        self.tx.commit()?;
        Ok(self.changes)
    }

    pub fn record_reception(&mut self, reception: &Reception) -> Result<Outcome> {
        let packet = match Packet::parse(&reception.frame) {
            Ok(packet) => packet,
            Err(e) => return Ok(Outcome::Malformed(e)),
        };
        let rx_at = reception.rx_at;
        let observer_id = self.upsert_observer(&reception.observer, rx_at)?;
        let hash = packet.hash();
        let (packet_id, new_packet) = match self.packet_id(&hash)? {
            Some(id) => (id, false),
            None => (self.insert_packet(&packet, &hash, rx_at)?, true),
        };

        let transport_codes =
            packet.transport_codes().map(|[a, b]| [a.to_le_bytes(), b.to_le_bytes()].concat());
        let inserted = self
            .tx
            .prepare_cached(
                "INSERT INTO observations (packet_id, observer_id, rx_at, header, transport_codes,
                                           path_len, path, snr, rssi, score, direction)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (observer_id, packet_id, rx_at) DO NOTHING",
            )?
            .execute(params![
                packet_id,
                observer_id,
                rx_at,
                packet.header().0,
                transport_codes,
                packet.path().len_byte(),
                packet.path().as_bytes(),
                reception.snr,
                reception.rssi,
                reception.score,
                reception.direction.as_deref().unwrap_or("rx"),
            ])?;
        if inserted == 0 {
            return Ok(Outcome::Duplicate);
        }
        self.changes.observations.push(self.tx.last_insert_rowid());

        self.tx
            .prepare_cached(
                "UPDATE packets SET first_seen_at = min(first_seen_at, ?2),
                                    last_seen_at = max(last_seen_at, ?2),
                                    observation_count = observation_count + 1
                 WHERE id = ?1",
            )?
            .execute(params![packet_id, rx_at])?;
        self.tx
            .prepare_cached(
                "UPDATE messages SET first_seen_at = ?2 WHERE packet_id = ?1 AND first_seen_at > ?2",
            )?
            .execute(params![packet_id, rx_at])?;
        if packet.payload_type() == PayloadType::Advert {
            self.touch_node(packet_id, rx_at)?;
        }
        Ok(Outcome::Recorded { packet_id, new_packet })
    }

    /// Returns false if this report was already stored.
    pub fn record_status(&mut self, report: &StatusReport) -> Result<bool> {
        let observer_id = self.upsert_observer(&report.observer, report.at)?;
        let inserted = self
            .tx
            .prepare_cached(
                "INSERT INTO observer_status (observer_id, at, status, model, firmware_version, radio,
                                              battery_mv, uptime_secs, noise_floor, tx_air_secs,
                                              rx_air_secs, packets_sent, packets_received,
                                              recv_errors, queue_len, raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT (observer_id, at) DO NOTHING",
            )?
            .execute(params![
                observer_id,
                report.at,
                report.status,
                report.model,
                report.firmware_version,
                report.radio,
                report.battery_mv,
                report.uptime_secs,
                report.noise_floor,
                report.tx_air_secs,
                report.rx_air_secs,
                report.packets_sent,
                report.packets_received,
                report.recv_errors,
                report.queue_len,
                report.raw,
            ])?;
        Ok(inserted > 0)
    }

    /// Returns false if this message was already stored. A repeat heard
    /// earlier than the stored copy replaces its reception details, so the
    /// result doesn't depend on replay order.
    pub fn record_direct_message(&mut self, message: &DirectMessage) -> Result<bool> {
        let observer_id = self.upsert_observer(&message.observer, message.received_at)?;
        let key = params![
            observer_id,
            message.sender_prefix,
            message.sender_timestamp,
            message.txt_type,
            message.body,
        ];
        let stored: Option<(i64, Micros)> = self
            .tx
            .prepare_cached(
                "SELECT id, received_at FROM direct_messages
                 WHERE observer_id = ?1 AND sender_prefix = ?2 AND sender_timestamp = ?3
                   AND txt_type = ?4 AND body = ?5",
            )?
            .query_row(key, |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        match stored {
            Some((id, received_at)) => {
                if message.received_at < received_at {
                    self.tx
                        .prepare_cached(
                            "UPDATE direct_messages SET received_at = ?2, path_len = ?3, snr = ?4
                             WHERE id = ?1",
                        )?
                        .execute(params![id, message.received_at, message.path_len, message.snr])?;
                }
                Ok(false)
            }
            None => {
                self.tx
                    .prepare_cached(
                        "INSERT INTO direct_messages (observer_id, received_at, sender_prefix,
                                                      path_len, txt_type, sender_timestamp,
                                                      signer_prefix, snr, body)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    )?
                    .execute(params![
                        observer_id,
                        message.received_at,
                        message.sender_prefix,
                        message.path_len,
                        message.txt_type,
                        message.sender_timestamp,
                        message.signer_prefix,
                        message.snr,
                        message.body,
                    ])?;
                Ok(true)
            }
        }
    }

    /// Returns false if this send was already stored.
    pub fn record_sent(&mut self, message: &SentMessage) -> Result<bool> {
        let observer_id = self.upsert_observer(&message.observer, message.sent_at)?;
        let (channel, packet_hash, recipient, recipient_name, expected_ack, timeout, flood) =
            match &message.to {
                SentTo::Channel { name, packet_hash } => {
                    (Some(name), Some(packet_hash.as_slice()), None, None, None, None, None)
                }
                SentTo::Node { pubkey, name, expected_ack, ack_timeout_ms, flood } => (
                    None,
                    None,
                    Some(pubkey.as_slice()),
                    name.as_ref(),
                    *expected_ack,
                    *ack_timeout_ms,
                    *flood,
                ),
            };
        let inserted = self
            .tx
            .prepare_cached(
                "INSERT INTO sent_messages (observer_id, sent_at, channel, recipient, recipient_name,
                                            body, sender_timestamp, packet_hash, expected_ack,
                                            ack_timeout_ms, flood, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (observer_id, sender_timestamp, body) DO NOTHING",
            )?
            .execute(params![
                observer_id,
                message.sent_at,
                channel,
                recipient,
                recipient_name,
                message.body,
                message.sender_timestamp,
                packet_hash,
                expected_ack,
                timeout,
                flood,
                message.error,
            ])?;
        Ok(inserted > 0)
    }

    /// Marks the newest unacknowledged send expecting this code as delivered.
    /// Returns false if none matched, such as for a repeated acknowledgement.
    pub fn record_ack(&mut self, ack: &Acknowledgement) -> Result<bool> {
        let observer_id = self.upsert_observer(&ack.observer, ack.at)?;
        let updated = self
            .tx
            .prepare_cached(
                "UPDATE sent_messages SET acked_at = ?3, round_trip_ms = ?4
                 WHERE id = (SELECT id FROM sent_messages
                             WHERE observer_id = ?1 AND expected_ack = ?2 AND acked_at IS NULL
                               AND error IS NULL AND sent_at <= ?3
                             ORDER BY sent_at DESC LIMIT 1)",
            )?
            .execute(params![observer_id, ack.ack, ack.at, ack.round_trip_ms])?;
        Ok(updated > 0)
    }

    /// Tries `key` on up to `limit` undecrypted packets carrying its hash,
    /// after packet id `after`. Returns what it did and the last id examined,
    /// or `None` once there are no packets left to try.
    pub(crate) fn backfill(
        &mut self,
        channel_id: i64,
        key: &ChannelKey,
        after: i64,
        limit: usize,
    ) -> Result<(Backfill, Option<i64>)> {
        let waiting: Vec<(i64, u8, Vec<u8>, Micros)> = self
            .tx
            .prepare_cached(
                "SELECT id, payload_type, payload, first_seen_at FROM packets
                 WHERE decode_state = 1 AND channel_hash = ?1 AND id > ?2
                 ORDER BY id LIMIT ?3",
            )?
            .query_map(params![key.hash(), after, limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut progress = Backfill::default();
        for (packet_id, payload_type, payload, first_seen_at) in &waiting {
            progress.checked += 1;
            let Ok(Payload::Group(group)) =
                Payload::parse(PayloadType::from_nibble(*payload_type), payload)
            else {
                continue;
            };
            let Some(plaintext) = key.decrypt(&group) else {
                continue;
            };
            progress.decrypted += 1;
            if self.record_decryption(*packet_id, channel_id, &group, &plaintext, *first_seen_at)? {
                progress.messages += 1;
            }
        }
        Ok((progress, waiting.last().map(|(id, ..)| *id)))
    }

    /// The name and region follow the most recent report.
    fn upsert_observer(&self, observer: &ObserverInfo, seen_at: Micros) -> Result<i64> {
        Ok(self
            .tx
            .prepare_cached(
                "INSERT INTO observers (pubkey, name, iata, kind, first_seen_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?5, ?4, ?4)
                 ON CONFLICT (pubkey) DO UPDATE SET
                     kind = excluded.kind,
                     name = CASE WHEN excluded.last_seen_at >= observers.last_seen_at
                                 THEN coalesce(excluded.name, observers.name)
                                 ELSE observers.name END,
                     iata = CASE WHEN excluded.last_seen_at >= observers.last_seen_at
                                 THEN coalesce(excluded.iata, observers.iata)
                                 ELSE observers.iata END,
                     first_seen_at = min(observers.first_seen_at, excluded.first_seen_at),
                     last_seen_at = max(observers.last_seen_at, excluded.last_seen_at)
                 RETURNING id",
            )?
            .query_row(
                params![
                    &observer.pubkey[..],
                    observer.name,
                    observer.iata,
                    seen_at,
                    observer.kind.as_str()
                ],
                |row| row.get(0),
            )?)
    }

    fn packet_id(&self, hash: &PacketHash) -> Result<Option<i64>> {
        Ok(self
            .tx
            .prepare_cached("SELECT id FROM packets WHERE hash = ?1")?
            .query_row([&hash.0[..]], |row| row.get(0))
            .optional()?)
    }

    fn insert_packet(
        &mut self,
        packet: &Packet<'_>,
        hash: &PacketHash,
        rx_at: Micros,
    ) -> Result<i64> {
        let decoded = packet.decode_payload();
        let (state, channel_hash) = match &decoded {
            Err(_) => (DecodeState::Malformed, None),
            Ok(Payload::Group(group)) => (DecodeState::Undecrypted, Some(group.channel_hash)),
            Ok(Payload::Addressed(_) | Payload::AnonReq(_)) => (DecodeState::Sealed, None),
            Ok(_) => (DecodeState::Cleartext, None),
        };
        self.tx
            .prepare_cached(
                "INSERT INTO packets (hash, payload_type, payload, first_seen_at, last_seen_at,
                                      channel_hash, decode_state)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)",
            )?
            .execute(params![
                &hash.0[..],
                packet.payload_type().nibble(),
                packet.payload(),
                rx_at,
                channel_hash,
                state as i64,
            ])?;
        let packet_id = self.tx.last_insert_rowid();
        self.changes.packets.push(packet_id);

        match decoded {
            Ok(Payload::Group(group)) => {
                let opened = self
                    .keys
                    .iter()
                    .find_map(|(id, key)| key.decrypt(&group).map(|plain| (*id, plain)));
                if let Some((channel_id, plaintext)) = opened {
                    self.record_decryption(packet_id, channel_id, &group, &plaintext, rx_at)?;
                }
            }
            Ok(Payload::Advert(advert)) => self.record_advert(packet_id, hash, &advert, rx_at)?,
            _ => {}
        }
        Ok(packet_id)
    }

    /// Marks a packet decrypted by a channel and, for GRP_TXT, records its
    /// message. Returns whether a message was recorded.
    fn record_decryption(
        &mut self,
        packet_id: i64,
        channel_id: i64,
        group: &GroupPayload<'_>,
        plaintext: &[u8],
        first_seen_at: Micros,
    ) -> Result<bool> {
        self.tx
            .prepare_cached("UPDATE packets SET channel_id = ?2, decode_state = ?3 WHERE id = ?1")?
            .execute(params![packet_id, channel_id, DecodeState::Decrypted as i64])?;

        // GRP_DATA decrypts too, but has no text layout to record yet.
        if group.kind != PayloadType::GrpTxt {
            return Ok(false);
        }
        let Some(message) = GroupText::parse(plaintext) else {
            return Ok(false);
        };
        let text = message.text_lossy();
        let (sender, body) = split_sender(&text);
        self.tx
            .prepare_cached(
                "INSERT INTO messages (packet_id, channel_id, first_seen_at, sender_timestamp,
                                       txt_type, attempt, sender, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                packet_id,
                channel_id,
                first_seen_at,
                message.sender_timestamp,
                message.txt_type,
                message.attempt,
                sender,
                body,
            ])?;
        self.changes.messages.push(self.tx.last_insert_rowid());
        Ok(true)
    }

    fn record_advert(
        &self,
        packet_id: i64,
        hash: &PacketHash,
        advert: &Advert<'_>,
        rx_at: Micros,
    ) -> Result<()> {
        let signature_ok = advert.verify();
        let app = advert.parse_app_data().ok();
        let flags = app.map(|app| app.flags);
        let role = flags.map(|flags| flags & 0x0F);
        let location = app.and_then(|app| app.location);
        let (lat_e6, lon_e6) = (location.map(|l| l.lat_e6), location.map(|l| l.lon_e6));
        let name = app
            .and_then(|app| app.name)
            .map(|name| String::from_utf8_lossy(name).trim_end_matches('\0').to_owned());

        self.tx
            .prepare_cached(
                "INSERT INTO adverts (packet_id, pubkey, adv_timestamp, flags, lat_e6, lon_e6, name,
                                      signature_ok)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                packet_id,
                &advert.pubkey[..],
                advert.timestamp,
                flags,
                lat_e6,
                lon_e6,
                name,
                signature_ok,
            ])?;
        // A forged advert must not rename or move a node.
        if !signature_ok {
            return Ok(());
        }

        let current: Option<(u32, Vec<u8>)> = self
            .tx
            .prepare_cached(
                "SELECT n.adv_timestamp, p.hash FROM nodes n
                 JOIN packets p ON p.id = n.advert_packet_id
                 WHERE n.pubkey = ?1",
            )?
            .query_row([&advert.pubkey[..]], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;

        match current {
            None => {
                self.tx
                    .prepare_cached(
                        "INSERT INTO nodes (pubkey, name, role, lat_e6, lon_e6, advert_packet_id,
                                            adv_timestamp, first_seen_at, last_seen_at, advert_count)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 1)",
                    )?
                    .execute(params![
                        &advert.pubkey[..],
                        name,
                        role,
                        lat_e6,
                        lon_e6,
                        packet_id,
                        advert.timestamp,
                        rx_at,
                    ])?;
            }
            // The hash breaks timestamp ties, so arrival order never decides.
            Some((adv_timestamp, current_hash))
                if (advert.timestamp, &hash.0[..]) > (adv_timestamp, current_hash.as_slice()) =>
            {
                self.tx
                    .prepare_cached(
                        "UPDATE nodes SET name = ?2, role = ?3, lat_e6 = ?4, lon_e6 = ?5,
                                          advert_packet_id = ?6, adv_timestamp = ?7,
                                          advert_count = advert_count + 1
                         WHERE pubkey = ?1",
                    )?
                    .execute(params![
                        &advert.pubkey[..],
                        name,
                        role,
                        lat_e6,
                        lon_e6,
                        packet_id,
                        advert.timestamp,
                    ])?;
            }
            Some(_) => {
                self.tx
                    .prepare_cached(
                        "UPDATE nodes SET advert_count = advert_count + 1 WHERE pubkey = ?1",
                    )?
                    .execute([&advert.pubkey[..]])?;
            }
        }
        Ok(())
    }

    /// Widens a node's seen window, and its name history, by one reception of
    /// one of its signed adverts.
    fn touch_node(&self, packet_id: i64, rx_at: Micros) -> Result<()> {
        let advert: Option<(Vec<u8>, Option<String>)> = self
            .tx
            .prepare_cached(
                "SELECT pubkey, name FROM adverts WHERE packet_id = ?1 AND signature_ok",
            )?
            .query_row([packet_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((pubkey, name)) = advert else {
            return Ok(());
        };

        self.tx
            .prepare_cached(
                "UPDATE nodes SET first_seen_at = min(first_seen_at, ?2),
                                  last_seen_at = max(last_seen_at, ?2)
                 WHERE pubkey = ?1",
            )?
            .execute(params![pubkey, rx_at])?;
        if let Some(name) = name {
            self.tx
                .prepare_cached(
                    "INSERT INTO node_names (pubkey, name, first_seen_at, last_seen_at)
                     VALUES (?1, ?2, ?3, ?3)
                     ON CONFLICT (pubkey, name) DO UPDATE SET
                         first_seen_at = min(node_names.first_seen_at, excluded.first_seen_at),
                         last_seen_at = max(node_names.last_seen_at, excluded.last_seen_at)",
                )?
                .execute(params![pubkey, name, rx_at])?;
        }
        Ok(())
    }
}
