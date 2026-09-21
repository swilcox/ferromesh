//! Messages in the meshcoretomqtt format, which observer firmware also uses.
//!
//! Topics are `<prefix>/<IATA>/<observer pubkey>/<packets|status|debug>`.
//! Numbers usually arrive as strings, so field parsing is lenient.

use anyhow::{Context, Result, bail};
use ferromesh_store::{Micros, ObserverInfo, ObserverKind, Reception, StatusReport};
use jiff::Timestamp;
use serde_json::{Map, Value};

use crate::source::Message;

pub fn parse(topic: &str, payload: &str) -> Result<Message> {
    let parts: Vec<&str> = topic.split('/').collect();
    let [_prefix, iata, pubkey, kind] = parts[..] else {
        bail!("unexpected topic layout");
    };
    if !matches!(kind, "packets" | "status") {
        return Ok(Message::Ignored);
    }

    let fields: Map<String, Value> =
        serde_json::from_str(payload).context("payload is not a JSON object")?;
    let observer = ObserverInfo {
        pubkey: observer_key(pubkey)?,
        name: text(&fields, "origin"),
        iata: Some(iata.to_owned()),
        kind: ObserverKind::Mqtt,
    };
    let at = timestamp(&fields)?;

    if kind == "packets" {
        let raw = fields.get("raw").and_then(Value::as_str).context("no raw frame")?;
        return Ok(Message::Packet(Reception {
            observer,
            rx_at: at,
            frame: hex::decode(raw).context("raw frame is not hex")?,
            snr: number(&fields, "SNR"),
            rssi: integer(&fields, "RSSI"),
            score: integer(&fields, "score"),
            direction: text(&fields, "direction"),
        }));
    }

    Ok(Message::Status(status_report(observer, at, &fields, payload)))
}

/// A status report from its JSON fields. Companion radios' reports use the
/// same fields.
pub(crate) fn status_report(
    observer: ObserverInfo,
    at: Micros,
    fields: &Map<String, Value>,
    payload: &str,
) -> StatusReport {
    let no_stats = Map::new();
    let stats = fields.get("stats").and_then(Value::as_object).unwrap_or(&no_stats);
    StatusReport {
        observer,
        at,
        status: text(fields, "status"),
        model: text(fields, "model"),
        firmware_version: text(fields, "firmware_version"),
        radio: text(fields, "radio"),
        battery_mv: integer(stats, "battery_mv"),
        uptime_secs: integer(stats, "uptime_secs"),
        noise_floor: integer(stats, "noise_floor"),
        tx_air_secs: integer(stats, "tx_air_secs"),
        rx_air_secs: integer(stats, "rx_air_secs"),
        packets_sent: integer(stats, "packets_sent"),
        packets_received: integer(stats, "packets_received"),
        recv_errors: integer(stats, "recv_errors"),
        queue_len: integer(stats, "queue_len"),
        raw: payload.to_owned(),
    }
}

pub(crate) fn observer_key(hex_key: &str) -> Result<[u8; 32]> {
    let mut key = [0; 32];
    hex::decode_to_slice(hex_key, &mut key)
        .with_context(|| format!("observer key {hex_key:?} is not 32 bytes of hex"))?;
    Ok(key)
}

fn timestamp(fields: &Map<String, Value>) -> Result<Micros> {
    let text = fields.get("timestamp").and_then(Value::as_str).context("no timestamp")?;
    Ok(text
        .parse::<Timestamp>()
        .with_context(|| format!("bad timestamp {text:?}"))?
        .as_microsecond())
}

pub(crate) fn text(fields: &Map<String, Value>, key: &str) -> Option<String> {
    fields.get(key)?.as_str().map(str::to_owned)
}

fn number(fields: &Map<String, Value>, key: &str) -> Option<f64> {
    match fields.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn integer(fields: &Map<String, Value>, key: &str) -> Option<i64> {
    match fields.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBSERVER: &str = "4D172767D319C09D15143D78DE49D082836987793639E8E6D914B5F70E8B4B0D";

    #[test]
    fn packet() {
        let payload = r#"{"timestamp": "2026-09-12T02:54:49.409094+00:00", "hash": "E16D26CF9414D2B3", "origin": "Tanyard", "type": "PACKET", "direction": "rx", "len": "36", "packet_type": "2", "route": "F", "payload_len": "20", "raw": "090E0362DE6DD04CC98AE66554FAA4A9D92F4E05E3B376DB8FDCD0E66158552B0E83C2B0", "SNR": "-1.2", "RSSI": "-86", "score": "537"}"#;
        let Message::Packet(reception) =
            parse(&format!("meshcore/BNA/{OBSERVER}/packets"), payload).unwrap()
        else {
            panic!("expected a packet");
        };
        assert_eq!(reception.observer.pubkey[..2], [0x4D, 0x17]);
        assert_eq!(reception.observer.name.as_deref(), Some("Tanyard"));
        assert_eq!(reception.observer.iata.as_deref(), Some("BNA"));
        assert_eq!(
            reception.rx_at,
            "2026-09-12T02:54:49.409094Z".parse::<Timestamp>().unwrap().as_microsecond()
        );
        assert_eq!(reception.frame.len(), 36);
        assert_eq!(
            (reception.snr, reception.rssi, reception.score),
            (Some(-1.2), Some(-86), Some(537))
        );
        assert_eq!(reception.direction.as_deref(), Some("rx"));
    }

    #[test]
    fn status() {
        let payload = r#"{"status": "online", "timestamp": "2026-09-12T21:21:09.716328+00:00", "origin": "Tanyard", "model": "Heltec V4 OLED", "firmware_version": "v1.17.1.3-observer-4226649", "radio": "910.525024,62.5,7,5", "repeat": "on", "stats": {"battery_mv": 4261, "uptime_secs": 1505, "packets_sent": 167, "packets_received": 372, "errors": 0, "queue_len": 0, "noise_floor": -104, "tx_air_secs": 59, "rx_air_secs": 123, "recv_errors": 198, "internal_heap": 201568}}"#;
        let Message::Status(report) =
            parse(&format!("meshcore/BNA/{OBSERVER}/status"), payload).unwrap()
        else {
            panic!("expected a status report");
        };
        assert_eq!(report.model.as_deref(), Some("Heltec V4 OLED"));
        assert_eq!(
            (report.battery_mv, report.noise_floor, report.recv_errors),
            (Some(4261), Some(-104), Some(198))
        );
        assert_eq!(report.raw, payload);
    }

    #[test]
    fn other_topics() {
        let debug = parse(&format!("meshcore/BNA/{OBSERVER}/debug"), "not json").unwrap();
        assert!(matches!(debug, Message::Ignored));
        assert!(parse("meshcore/packets", "{}").is_err());
        let not_hex = r#"{"timestamp": "2026-09-12T00:00:00Z", "raw": ""}"#;
        assert!(parse("meshcore/BNA/nothex/packets", not_hex).is_err());
    }
}
