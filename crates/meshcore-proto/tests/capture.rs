//! Golden test: parse a real capture and compare against the independent
//! Python decoder (see `tools/gen_fixture.py`). Mismatches are collected and
//! reported together, so one run shows the whole picture.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::LazyLock;

use meshcore_proto::{ChannelKey, GroupText, Keyring, Packet, Payload, PayloadType};
use serde::Deserialize;

#[derive(Deserialize)]
struct Record {
    hash: String,
    len: usize,
    packet_type: u8,
    route: String,
    payload_len: usize,
    raw: String,
    /// Python's advert decode; `None` for non-adverts or ones it rejected.
    advert: Option<ExpectedAdvert>,
    /// Python's GRP_TXT decrypt; `None` for other types or unknown channels.
    grp: Option<ExpectedGroup>,
}

#[derive(Deserialize)]
struct ExpectedAdvert {
    pubkey: String,
    adv_timestamp: u32,
    flags: u8,
    lat: Option<f64>,
    lon: Option<f64>,
    name: String,
}

#[derive(Deserialize)]
struct ExpectedGroup {
    channel: String,
    sender_timestamp: u32,
    attempt: u8,
    txt_type: u8,
    text: String,
}

#[derive(Deserialize)]
struct Summary {
    packets: usize,
    adverts: usize,
    grp_decrypted: usize,
    grp_undecrypted: usize,
    unique_messages: usize,
}

#[derive(Deserialize)]
struct ChannelEntry {
    name: String,
}

struct Capture {
    records: Vec<(Record, Vec<u8>)>,
    summary: Summary,
}

static CAPTURE: LazyLock<Capture> = LazyLock::new(|| Capture {
    records: fixture("capture.jsonl")
        .lines()
        .map(|line| {
            let record: Record = serde_json::from_str(line).expect("fixture record");
            let raw = hex::decode(&record.raw).expect("fixture hex");
            (record, raw)
        })
        .collect(),
    summary: serde_json::from_str(&fixture("summary.json")).expect("fixture summary"),
});

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn records_of(kind: PayloadType) -> impl Iterator<Item = &'static (Record, Vec<u8>)> {
    CAPTURE.records.iter().filter(move |(record, _)| record.packet_type == kind.nibble())
}

/// The same channel list the Python decoder used: public first, then the file.
fn keyring() -> Keyring {
    let entries: Vec<ChannelEntry> =
        serde_json::from_str(&fixture("channels.json")).expect("fixture channels");
    let mut keyring = Keyring::with_public();
    for entry in entries {
        keyring.add(&entry.name, ChannelKey::from_hashtag(&entry.name));
    }
    keyring
}

/// Python's `bytes.decode("utf-8", "ignore")`: invalid bytes vanish rather
/// than becoming U+FFFD.
fn py_decode_ignore(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\u{FFFD}', "")
}

fn assert_no_failures(what: &str, failures: &[String]) {
    const SHOWN: usize = 10;
    assert!(
        failures.is_empty(),
        "{} {what}, first {}:\n  {}",
        failures.len(),
        SHOWN.min(failures.len()),
        failures[..SHOWN.min(failures.len())].join("\n  ")
    );
}

#[test]
fn framing_matches_observer_metadata() {
    assert_eq!(CAPTURE.records.len(), CAPTURE.summary.packets);

    let mut failures = Vec::new();
    for (record, raw) in &CAPTURE.records {
        let packet = match Packet::parse(raw) {
            Ok(packet) => packet,
            Err(e) => {
                failures.push(format!("{}: {e}", record.hash));
                continue;
            }
        };
        let flood = match record.route.as_str() {
            "F" => true,
            "D" => false,
            other => {
                failures.push(format!("{}: unknown route {other:?}", record.hash));
                continue;
            }
        };
        let checks = [
            ("len", raw.len() == record.len),
            ("payload_len", packet.payload().len() == record.payload_len),
            ("packet_type", packet.payload_type().nibble() == record.packet_type),
            ("route", packet.route_type().is_flood() == flood),
            ("hash", packet.hash().to_string() == record.hash),
        ];
        for (field, ok) in checks {
            if !ok {
                failures.push(format!("{}: {field} mismatch in {packet:?}", record.hash));
            }
        }
    }
    assert_no_failures("framing mismatches", &failures);
}

#[test]
fn every_payload_parses() {
    let failures: Vec<String> = CAPTURE
        .records
        .iter()
        .filter_map(|(record, raw)| {
            // Framing failures are reported by the framing test.
            let packet = Packet::parse(raw).ok()?;
            let e = packet.decode_payload().err()?;
            Some(format!("{}: {e}", record.hash))
        })
        .collect();
    assert_no_failures("payload parse failures", &failures);
}

#[test]
fn adverts_match_python_and_verify() {
    let mut failures = Vec::new();
    let mut decoded = 0;
    for (record, raw) in records_of(PayloadType::Advert) {
        let Ok(Payload::Advert(advert)) = Packet::parse(raw).and_then(|p| p.decode_payload())
        else {
            failures.push(format!("{}: not parseable as an advert", record.hash));
            continue;
        };
        match (&record.advert, advert.parse_app_data().ok()) {
            (None, None) => {}
            (None, Some(_)) => {
                failures.push(format!("{}: Python rejected, we accepted", record.hash))
            }
            (Some(_), None) => {
                failures.push(format!("{}: we rejected, Python accepted", record.hash))
            }
            (Some(want), Some(app)) => {
                decoded += 1;
                let name = app
                    .name
                    .map(|bytes| py_decode_ignore(bytes).trim_matches('\0').trim().to_owned())
                    .unwrap_or_default();
                let got = (
                    hex::encode(advert.pubkey),
                    advert.timestamp,
                    app.flags,
                    app.location.map(|l| l.lat()),
                    app.location.map(|l| l.lon()),
                    name,
                );
                let expected = (
                    want.pubkey.clone(),
                    want.adv_timestamp,
                    want.flags,
                    want.lat,
                    want.lon,
                    want.name.clone(),
                );
                if got != expected {
                    failures.push(format!("{}: got {got:?}, Python {expected:?}", record.hash));
                }
                if !advert.verify() {
                    failures.push(format!("{}: bad signature from {:?}", record.hash, want.name));
                }
            }
        }
    }
    assert_no_failures("advert mismatches", &failures);
    assert_eq!(decoded, CAPTURE.summary.adverts);
}

#[test]
fn tampered_advert_fails_verification() {
    let (_, raw) = records_of(PayloadType::Advert)
        .find(|(record, _)| record.advert.is_some())
        .expect("capture has adverts");
    let mut raw = raw.clone();
    // The payload ends the frame, so the last byte is inside app_data.
    *raw.last_mut().unwrap() ^= 0x01;

    let Payload::Advert(advert) = Packet::parse(&raw).unwrap().decode_payload().unwrap() else {
        panic!("still an advert");
    };
    assert!(!advert.verify());
}

#[test]
fn channel_messages_match_python() {
    let keyring = keyring();
    let mut failures = Vec::new();
    let (mut decrypted, mut undecrypted) = (0, 0);
    let mut unique = HashSet::new();

    for (record, raw) in records_of(PayloadType::GrpTxt) {
        let Ok(Payload::Group(group)) = Packet::parse(raw).and_then(|p| p.decode_payload()) else {
            failures.push(format!("{}: not parseable as a group payload", record.hash));
            continue;
        };
        let got = keyring.decrypt(&group).and_then(|(channel, plaintext)| {
            Some((channel.name.clone(), GroupText::parse(&plaintext)?))
        });

        match (&record.grp, got) {
            (None, None) => undecrypted += 1,
            (Some(want), None) => failures.push(format!(
                "{}: Python decrypted [{}] {:?}, we did not",
                record.hash, want.channel, want.text
            )),
            (None, Some((channel, message))) => failures.push(format!(
                "{}: we decrypted [{channel}] {:?}, Python did not",
                record.hash,
                message.text_lossy()
            )),
            (Some(want), Some((channel, message))) => {
                decrypted += 1;
                let text = py_decode_ignore(&message.text);
                let got = (
                    channel.as_str(),
                    message.sender_timestamp,
                    message.attempt,
                    message.txt_type,
                    text.as_str(),
                );
                let expected = (
                    want.channel.as_str(),
                    want.sender_timestamp,
                    want.attempt,
                    want.txt_type,
                    want.text.as_str(),
                );
                if got != expected {
                    failures.push(format!("{}: got {got:?}, Python {expected:?}", record.hash));
                }
                unique.insert((channel, message.sender_timestamp, text));
            }
        }
    }

    assert_no_failures("channel decode mismatches", &failures);
    assert_eq!(decrypted, CAPTURE.summary.grp_decrypted);
    assert_eq!(undecrypted, CAPTURE.summary.grp_undecrypted);
    assert_eq!(unique.len(), CAPTURE.summary.unique_messages);
}
