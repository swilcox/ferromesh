//! Nodes, and packet detail with every reception's frame rebuilt.

use ed25519_dalek::{Signer, SigningKey};
use ferromesh_store::{ChannelKind, ObserverInfo, ObserverKind, Reception, Store};
use meshcore_proto::{ChannelKey, GroupText, Packet};

fn observer(seed: u8, name: &str) -> ObserverInfo {
    ObserverInfo {
        pubkey: [seed; 32],
        name: Some(name.into()),
        iata: Some("BNA".into()),
        kind: ObserverKind::Mqtt,
    }
}

fn reception(index: usize, observer: &ObserverInfo, frame: &[u8], snr: f64) -> Reception {
    Reception {
        observer: observer.clone(),
        rx_at: 1_789_000_000_000_000 + index as i64 * 1_000_000,
        frame: frame.to_vec(),
        snr: Some(snr),
        rssi: Some(-90),
        score: None,
        direction: None,
    }
}

/// A signed flood advert with a location and name.
fn advert(seed: u8, timestamp: u32, flags: u8, name: &str) -> Vec<u8> {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let pubkey = key.verifying_key().to_bytes();
    let mut app_data = vec![flags];
    if flags & 0x10 != 0 {
        app_data.extend(36_100_000i32.to_le_bytes());
        app_data.extend((-86_800_000i32).to_le_bytes());
    }
    app_data.extend_from_slice(name.as_bytes());
    let mut signed = pubkey.to_vec();
    signed.extend(timestamp.to_le_bytes());
    signed.extend(&app_data);

    let mut frame = vec![0x11, 0x00];
    frame.extend(pubkey);
    frame.extend(timestamp.to_le_bytes());
    frame.extend(key.sign(&signed).to_bytes());
    frame.extend(app_data);
    frame
}

fn ingest(store: &mut Store, receptions: &[Reception]) {
    store
        .write(|batch| receptions.iter().try_for_each(|r| batch.record_reception(r).map(drop)))
        .unwrap();
}

#[test]
fn packet_detail_rebuilds_every_frame() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .add_channel("#test", &ChannelKey::from_hashtag("#test"), ChannelKind::Hashtag, 0)
        .unwrap();

    let message =
        GroupText { sender_timestamp: 1, txt_type: 0, attempt: 0, text: b"Bob: hi".to_vec() };
    let payload = ChannelKey::from_hashtag("#test").encrypt(&message.to_plaintext());
    let direct = [&[0x15, 0x00][..], &payload].concat();
    let relayed = [&[0x15, 0x01, 0x11][..], &payload].concat();
    // Transport flood: header route 0, then two transport codes before the path.
    let transported =
        [&[0x14, 0x34, 0x12, 0x78, 0x56, 0x42, 0x11, 0x22, 0x33, 0x44][..], &payload].concat();

    let (tanyard, ridge) = (observer(1, "Tanyard"), observer(2, "Ridge"));
    ingest(
        &mut store,
        &[
            reception(0, &tanyard, &direct, 5.0),
            reception(1, &ridge, &relayed, -4.0),
            reception(2, &tanyard, &transported, -9.5),
        ],
    );

    let hash = Packet::parse(&direct).unwrap().hash();
    let detail = store.packet_detail(&hash.0).unwrap().unwrap();
    assert_eq!(detail.packet.text.as_deref(), Some("Bob: hi"));
    assert_eq!(detail.packet.heard, 3);
    let frames: Vec<Vec<u8>> =
        detail.receptions.iter().map(|r| hex::decode(&r.frame).unwrap()).collect();
    assert_eq!(frames, [direct, relayed, transported]);
    let observers: Vec<_> =
        detail.receptions.iter().map(|r| (r.observer.as_str(), r.snr)).collect();
    assert_eq!(observers, [("Tanyard", Some(5.0)), ("Ridge", Some(-4.0)), ("Tanyard", Some(-9.5))]);

    assert!(store.packet_detail(&[0; 8]).unwrap().is_none());
}

#[test]
fn nodes_most_recently_heard_first() {
    let mut store = Store::open_in_memory().unwrap();
    let tanyard = observer(1, "Tanyard");
    ingest(
        &mut store,
        &[
            reception(0, &tanyard, &advert(9, 100, 0x92, "Hilltop"), 3.0),
            reception(1, &tanyard, &advert(10, 100, 0x81, "Chatty"), 1.0),
            reception(2, &tanyard, &advert(9, 200, 0x92, "Hilltop 2"), 2.0),
        ],
    );

    let nodes = store.nodes(10).unwrap();
    let summary: Vec<_> = nodes
        .iter()
        .map(|n| (n.name.as_deref().unwrap(), n.role.as_deref().unwrap(), n.adverts))
        .collect();
    assert_eq!(summary, [("Hilltop 2", "repeater", 2), ("Chatty", "chat", 1)]);
    assert_eq!((nodes[0].lat, nodes[0].lon), (Some(36.1), Some(-86.8)));
    assert_eq!((nodes[1].lat, nodes[1].lon), (None, None));
    assert!(nodes[0].first_seen_at < nodes[0].last_seen_at);
    assert_eq!(store.nodes(1).unwrap().len(), 1);
}
