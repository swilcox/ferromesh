//! Companion frames as raw records, and back.
//!
//! Topics are `companion/<radio pubkey>/<rx|message|status>`. Packets and
//! messages keep the radio's frame verbatim, in hex; status reports use the
//! same JSON fields as observer firmware, plus the frames they came from.

use anyhow::{Context, Result, bail};
use ferromesh_store::{DirectMessage, ObserverInfo, Reception};
use jiff::Timestamp;
use meshcore_proto::companion::{Frame, Stats, code};
use serde_json::{Map, Value, json};

use super::TOPIC_PREFIX;
use super::session::{Identity, Received};
use crate::meshcoretomqtt::{observer_key, status_report, text};
use crate::rawlog::RawRecord;
use crate::source::Message;

/// A received packet or queued message as a raw record, or `None` for
/// frames we don't keep.
pub fn received(identity: &Identity, source: &str, received: &Received) -> Option<RawRecord> {
    let kind = match *received.frame.first()? {
        code::PUSH_LOG_RX_DATA => "rx",
        code::CONTACT_MSG_RECV
        | code::CONTACT_MSG_RECV_V3
        | code::CHANNEL_MSG_RECV
        | code::CHANNEL_MSG_RECV_V3
        | code::CHANNEL_DATA_RECV => "message",
        _ => return None,
    };
    let payload =
        json!({ "origin": identity.info.name, "frame": hex::encode_upper(&received.frame) });
    Some(record(identity, source, kind, received.at, payload))
}

/// Three stats frames (core, radio, packets) as one status report.
pub fn status(identity: &Identity, source: &str, at: Timestamp, frames: &[Vec<u8>]) -> RawRecord {
    let mut stats = Map::new();
    for frame in frames {
        match Frame::parse(frame) {
            Ok(Frame::Stats(Stats::Core { battery_mv, uptime_secs, queue_len, .. })) => {
                stats.insert("battery_mv".into(), battery_mv.into());
                stats.insert("uptime_secs".into(), uptime_secs.into());
                stats.insert("queue_len".into(), queue_len.into());
            }
            Ok(Frame::Stats(Stats::Radio { noise_floor, tx_air_secs, rx_air_secs, .. })) => {
                stats.insert("noise_floor".into(), noise_floor.into());
                stats.insert("tx_air_secs".into(), tx_air_secs.into());
                stats.insert("rx_air_secs".into(), rx_air_secs.into());
            }
            Ok(Frame::Stats(Stats::Packets { received, sent, receive_errors, .. })) => {
                stats.insert("packets_received".into(), received.into());
                stats.insert("packets_sent".into(), sent.into());
                stats.insert("recv_errors".into(), receive_errors.into());
            }
            _ => {}
        }
    }
    let frames: Vec<String> = frames.iter().map(hex::encode_upper).collect();
    let payload = json!({
        "status": "online",
        "origin": identity.info.name,
        "model": identity.device.model,
        "firmware_version": identity.device.version,
        "radio": identity.info.radio(),
        "stats": stats,
        "frames": frames,
    });
    record(identity, source, "status", at, payload)
}

fn record(
    identity: &Identity,
    source: &str,
    kind: &str,
    at: Timestamp,
    payload: Value,
) -> RawRecord {
    RawRecord {
        received_at: at,
        source: source.to_owned(),
        topic: format!("{TOPIC_PREFIX}{}/{kind}", hex::encode_upper(identity.info.pubkey)),
        payload: payload.to_string(),
    }
}

pub fn parse(record: &RawRecord) -> Result<Message> {
    let rest = record.topic.strip_prefix(TOPIC_PREFIX).context("not a companion topic")?;
    let Some((pubkey, kind)) = rest.split_once('/') else {
        bail!("unexpected topic layout");
    };
    let fields: Map<String, Value> =
        serde_json::from_str(&record.payload).context("payload is not a JSON object")?;
    let observer =
        ObserverInfo { pubkey: observer_key(pubkey)?, name: text(&fields, "origin"), iata: None };
    let at = record.received_at.as_microsecond();

    if kind == "status" {
        return Ok(Message::Status(status_report(observer, at, &fields, &record.payload)));
    }
    let frame = fields.get("frame").and_then(Value::as_str).context("no frame")?;
    let frame = hex::decode(frame).context("frame is not hex")?;
    match Frame::parse(&frame).context("unreadable frame")? {
        Frame::RxLog(rx) => Ok(Message::Packet(Reception {
            observer,
            rx_at: at,
            frame: rx.raw.to_vec(),
            snr: Some(rx.snr),
            rssi: Some(i64::from(rx.rssi)),
            score: None,
            direction: None,
        })),
        Frame::ContactMessage(message) => Ok(Message::Direct(DirectMessage {
            observer,
            received_at: at,
            sender_prefix: message.sender_prefix,
            path_len: message.path_len,
            txt_type: message.txt_type,
            sender_timestamp: message.sender_timestamp,
            signer_prefix: message.signer_prefix,
            snr: message.snr,
            body: String::from_utf8_lossy(message.text).into_owned(),
        })),
        // The radio decrypts channels in its own slots, but the same packets
        // arrive as receptions and are decoded with the server's channels.
        _ => Ok(Message::Ignored),
    }
}

#[cfg(test)]
mod tests {
    use meshcore_proto::companion::{DeviceInfo, SelfInfo};

    use super::*;

    fn identity() -> Identity {
        Identity {
            info: SelfInfo {
                advert_type: 1,
                tx_power_dbm: 10,
                max_tx_power_dbm: 22,
                pubkey: [0xAB; 32],
                lat_e6: 0,
                lon_e6: 0,
                freq_khz: 910_525,
                bandwidth_hz: 62_500,
                spreading_factor: 7,
                coding_rate: 5,
                name: "desk".into(),
            },
            device: DeviceInfo {
                protocol_version: 13,
                max_contacts: 350,
                max_channels: 40,
                build_date: "14-Aug-2026".into(),
                model: "Heltec V4.3 OLED".into(),
                version: "v1.17.1".into(),
            },
        }
    }

    fn at() -> Timestamp {
        "2026-09-19T16:00:00Z".parse().unwrap()
    }

    fn round_trip(frame: Vec<u8>) -> (RawRecord, Message) {
        let record =
            received(&identity(), "companion:test", &Received { at: at(), frame }).unwrap();
        let message = parse(&record).unwrap();
        (record, message)
    }

    #[test]
    fn receptions() {
        let (record, message) =
            round_trip(vec![code::PUSH_LOG_RX_DATA, 17, (-77i8) as u8, 0x15, 0x00, 0xAA]);
        assert_eq!(record.topic, format!("companion/{}/rx", "AB".repeat(32)));
        let Message::Packet(reception) = message else { panic!("{message:?}") };
        assert_eq!(reception.observer.name.as_deref(), Some("desk"));
        assert_eq!(reception.observer.iata, None);
        assert_eq!((reception.snr, reception.rssi), (Some(4.25), Some(-77)));
        assert_eq!(reception.frame, [0x15, 0x00, 0xAA]);
        assert_eq!(reception.rx_at, at().as_microsecond());
    }

    #[test]
    fn direct_and_channel_messages() {
        let mut dm = vec![code::CONTACT_MSG_RECV_V3, 8, 0, 0, 1, 2, 3, 4, 5, 6, 0xFF, 0];
        dm.extend(1_789_000_000u32.to_le_bytes());
        dm.extend("héllo".as_bytes());
        let (_, message) = round_trip(dm);
        let Message::Direct(direct) = message else { panic!("{message:?}") };
        assert_eq!((direct.body.as_str(), direct.path_len, direct.snr), ("héllo", None, Some(2.0)));
        assert_eq!(direct.sender_prefix, [1, 2, 3, 4, 5, 6]);

        let mut channel = vec![code::CHANNEL_MSG_RECV_V3, 8, 0, 0, 0, 1, 0, 0, 0, 0, 0];
        channel.extend(b"Bob: hi");
        let (record, message) = round_trip(channel);
        assert!(record.topic.ends_with("/message"));
        assert!(matches!(message, Message::Ignored));

        let other = Received { at: at(), frame: vec![code::PUSH_NEW_ADVERT] };
        assert!(received(&identity(), "companion:test", &other).is_none());
    }

    #[test]
    fn status_reports() {
        let core =
            [&[code::STATS, 0][..], &4296u16.to_le_bytes(), &711u32.to_le_bytes(), &[0, 0, 0]]
                .concat();
        let mut radio = vec![code::STATS, 1];
        radio.extend((-82i16).to_le_bytes());
        radio.extend([0xE4, 50]);
        radio.extend(0u32.to_le_bytes());
        radio.extend(8u32.to_le_bytes());
        let mut packets = vec![code::STATS, 2];
        for n in [27u32, 0, 0, 0, 27, 0, 1] {
            packets.extend(n.to_le_bytes());
        }
        let record = status(&identity(), "companion:test", at(), &[core, radio, packets]);
        let Message::Status(report) = parse(&record).unwrap() else { panic!() };
        assert_eq!(report.model.as_deref(), Some("Heltec V4.3 OLED"));
        assert_eq!(report.radio.as_deref(), Some("910.525,62.5,7,5"));
        assert_eq!(
            (report.battery_mv, report.uptime_secs, report.noise_floor, report.rx_air_secs),
            (Some(4296), Some(711), Some(-82), Some(8))
        );
        assert_eq!((report.packets_received, report.recv_errors), (Some(27), Some(1)));
        assert_eq!(report.raw, record.payload);
    }

    #[test]
    fn bad_records() {
        let bad = |topic: &str, payload: &str| RawRecord {
            received_at: at(),
            source: "test".into(),
            topic: topic.into(),
            payload: payload.into(),
        };
        let key = "AB".repeat(32);
        assert!(parse(&bad("companion/nope", "{}")).is_err());
        assert!(parse(&bad(&format!("companion/{key}/rx"), r#"{"frame":"zz"}"#)).is_err());
        assert!(parse(&bad(&format!("companion/{key}/rx"), r#"{"frame":"88"}"#)).is_err());
    }
}
