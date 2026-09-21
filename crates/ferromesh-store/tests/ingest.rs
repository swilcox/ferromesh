//! Ingest the meshcore-proto capture fixture and check what the store derives.

use std::collections::HashSet;
use std::path::Path;

use ferromesh_store::{
    ChannelKind, ObserverInfo, ObserverKind, Outcome, Reception, StatusReport, Store,
};
use meshcore_proto::ChannelKey;
use serde::Deserialize;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../meshcore-proto/tests/fixtures");

#[derive(Deserialize)]
struct Record {
    hash: String,
    packet_type: u8,
    raw: String,
    advert: Option<ExpectedAdvert>,
    grp: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ExpectedAdvert {
    pubkey: String,
}

#[derive(Deserialize)]
struct ChannelEntry {
    name: String,
}

struct Fixture {
    records: Vec<Record>,
    receptions: Vec<Reception>,
    channels: Vec<String>,
}

/// The fixture is real traffic, so it isn't committed; tools/gen_fixture.py
/// builds it. Without it these tests skip, unless FERROMESH_REQUIRE_FIXTURES
/// is set.
fn fixture() -> Option<Fixture> {
    if !Path::new(FIXTURES).join("capture.jsonl").exists() {
        assert!(
            std::env::var_os("FERROMESH_REQUIRE_FIXTURES").is_none(),
            "no fixture in {FIXTURES}; run tools/gen_fixture.py"
        );
        eprintln!("skipping: no fixture in {FIXTURES} (see tools/gen_fixture.py)");
        return None;
    }
    let read = |name: &str| std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap();
    let records: Vec<Record> =
        read("capture.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let channels = serde_json::from_str::<Vec<ChannelEntry>>(&read("channels.json"))
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    let observer = tanyard();
    // The fixture keeps no receive times; one second apart makes every line a
    // distinct observation.
    let receptions = records
        .iter()
        .enumerate()
        .map(|(index, record)| Reception {
            observer: observer.clone(),
            rx_at: 1_789_000_000_000_000 + index as i64 * 1_000_000,
            frame: hex::decode(&record.raw).unwrap(),
            snr: Some(-1.5),
            rssi: Some(-90),
            score: None,
            direction: None,
        })
        .collect();
    Some(Fixture { records, receptions, channels })
}

fn tanyard() -> ObserverInfo {
    ObserverInfo {
        pubkey: [7; 32],
        name: Some("Tanyard".into()),
        iata: Some("BNA".into()),
        kind: ObserverKind::Mqtt,
    }
}

fn store_with(channels: impl IntoIterator<Item = impl AsRef<str>>) -> Store {
    let mut store = Store::open_in_memory().unwrap();
    for name in channels {
        let name = name.as_ref();
        store.add_channel(name, &ChannelKey::from_hashtag(name), ChannelKind::Hashtag, 0).unwrap();
    }
    store
}

fn ingest<'a>(
    store: &mut Store,
    receptions: impl IntoIterator<Item = &'a Reception>,
) -> Vec<Outcome> {
    store
        .write(|batch| receptions.into_iter().map(|r| batch.record_reception(r)).collect())
        .unwrap()
}

fn distinct<'a>(hashes: impl Iterator<Item = &'a str>) -> i64 {
    hashes.collect::<HashSet<_>>().len() as i64
}

#[test]
fn counts_match_the_capture() {
    let Some(f) = fixture() else { return };
    let mut store = store_with(&f.channels);
    let outcomes = ingest(&mut store, &f.receptions);
    assert!(outcomes.iter().all(|outcome| matches!(outcome, Outcome::Recorded { .. })));

    let hashes_where = |keep: &dyn Fn(&Record) -> bool| {
        distinct(f.records.iter().filter(|r| keep(r)).map(|r| r.hash.as_str()))
    };
    let counts = store.counts().unwrap();
    assert_eq!(counts.observers, 1);
    assert_eq!(counts.observations, f.records.len() as i64);
    assert_eq!(counts.packets, hashes_where(&|_| true));
    assert_eq!(counts.messages, hashes_where(&|r| r.grp.is_some()));
    assert_eq!(counts.adverts, hashes_where(&|r| r.packet_type == 4));
    assert_eq!(counts.channels, f.channels.len() as i64 + 1);
    // GRP_DATA packets count as undecrypted too, but Python never tried them.
    assert!(counts.undecrypted >= hashes_where(&|r| r.packet_type == 5 && r.grp.is_none()));

    let pubkeys: HashSet<&str> =
        f.records.iter().filter_map(|r| r.advert.as_ref()).map(|a| a.pubkey.as_str()).collect();
    assert_eq!(counts.nodes, pubkeys.len() as i64);
}

#[test]
fn reingesting_changes_nothing() {
    let Some(f) = fixture() else { return };
    let mut store = store_with(&f.channels);
    ingest(&mut store, &f.receptions);
    let (digest, counts) = (store.digest().unwrap(), store.counts().unwrap());

    let outcomes = ingest(&mut store, &f.receptions);
    assert!(outcomes.iter().all(|outcome| *outcome == Outcome::Duplicate));
    assert_eq!(store.digest().unwrap(), digest);
    assert_eq!(store.counts().unwrap(), counts);
}

#[test]
fn arrival_order_does_not_matter() {
    let Some(f) = fixture() else { return };
    let mut forward = store_with(&f.channels);
    ingest(&mut forward, &f.receptions);

    let mut reversed = store_with(f.channels.iter().rev());
    ingest(&mut reversed, f.receptions.iter().rev());
    assert_eq!(forward.digest().unwrap(), reversed.digest().unwrap());

    // And the digest does notice missing data.
    let mut partial = store_with(&f.channels);
    ingest(&mut partial, &f.receptions[..f.receptions.len() - 1]);
    assert_ne!(forward.digest().unwrap(), partial.digest().unwrap());
}

#[test]
fn status_reports_are_stored_once() {
    let mut store = Store::open_in_memory().unwrap();
    let report = StatusReport {
        observer: tanyard(),
        at: 1_789_000_000_000_000,
        status: Some("online".into()),
        model: Some("Heltec V4 OLED".into()),
        firmware_version: None,
        radio: None,
        battery_mv: Some(4261),
        uptime_secs: Some(1505),
        noise_floor: Some(-104),
        tx_air_secs: None,
        rx_air_secs: None,
        packets_sent: None,
        packets_received: None,
        recv_errors: None,
        queue_len: None,
        raw: "{}".into(),
    };
    assert!(store.write(|batch| batch.record_status(&report)).unwrap());
    assert!(!store.write(|batch| batch.record_status(&report)).unwrap());
    assert_eq!(store.counts().unwrap().statuses, 1);
}
