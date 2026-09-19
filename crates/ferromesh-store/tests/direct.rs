//! Direct messages handed over by a companion radio.

use ed25519_dalek::{Signer, SigningKey};
use ferromesh_store::{DirectMessage, ObserverInfo, Reception, Store};

const T0: i64 = 1_789_000_000_000_000;

fn companion() -> ObserverInfo {
    ObserverInfo { pubkey: [7; 32], name: Some("desk".into()), iata: None }
}

fn direct(received_at: i64, sender_prefix: [u8; 6], body: &str, snr: f64) -> DirectMessage {
    DirectMessage {
        observer: companion(),
        received_at,
        sender_prefix,
        path_len: Some(2),
        txt_type: 0,
        sender_timestamp: 1_789_000_000,
        signer_prefix: None,
        snr: Some(snr),
        body: body.into(),
    }
}

/// A signed advert naming the node whose key comes from `seed`.
fn advert(seed: u8, name: &str) -> ([u8; 32], Vec<u8>) {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let pubkey = key.verifying_key().to_bytes();
    let timestamp = 1_789_000_000u32;
    let app_data = [&[0x81][..], name.as_bytes()].concat();
    let signed = [&pubkey[..], &timestamp.to_le_bytes(), &app_data].concat();
    let frame = [
        &[0x11, 0x00][..],
        &pubkey,
        &timestamp.to_le_bytes(),
        &key.sign(&signed).to_bytes(),
        &app_data,
    ]
    .concat();
    (pubkey, frame)
}

#[test]
fn stored_once_keeping_the_earliest_copy_in_any_order() {
    let early = direct(T0, [1; 6], "hello", -2.0);
    let late = direct(T0 + 5_000_000, [1; 6], "hello", 8.0);

    let mut forwards = Store::open_in_memory().unwrap();
    assert!(forwards.write(|batch| batch.record_direct_message(&early)).unwrap());
    assert!(!forwards.write(|batch| batch.record_direct_message(&late)).unwrap());

    let mut backwards = Store::open_in_memory().unwrap();
    assert!(backwards.write(|batch| batch.record_direct_message(&late)).unwrap());
    assert!(!backwards.write(|batch| batch.record_direct_message(&early)).unwrap());

    assert_eq!(forwards.digest().unwrap(), backwards.digest().unwrap());
    let stored = backwards.direct_messages(10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].received_at.as_microsecond(), T0);
    assert_eq!((stored[0].snr, stored[0].to.as_str()), (Some(-2.0), "desk"));
    assert_eq!(backwards.counts().unwrap().direct_messages, 1);
}

#[test]
fn senders_are_named_by_key_prefix() {
    let (pubkey, frame) = advert(3, "Hilltop");
    let reception = Reception {
        observer: companion(),
        rx_at: T0,
        frame,
        snr: None,
        rssi: None,
        score: None,
        direction: None,
    };
    let mut store = Store::open_in_memory().unwrap();
    store.write(|batch| batch.record_reception(&reception)).unwrap();

    let known = direct(T0 + 1, pubkey[..6].try_into().unwrap(), "from a known node", 1.0);
    let mut unknown = direct(T0 + 2, [9; 6], "from a stranger", 1.0);
    unknown.path_len = None;
    store.write(|batch| batch.record_direct_message(&known)).unwrap();
    store.write(|batch| batch.record_direct_message(&unknown)).unwrap();

    let listed = store.direct_messages(10).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!((listed[0].sender.as_deref(), listed[0].hops), (None, None));
    assert_eq!(listed[0].sender_prefix, "090909090909");
    assert_eq!((listed[1].sender.as_deref(), listed[1].hops), (Some("Hilltop"), Some(2)));
    assert_eq!(store.direct_messages(1).unwrap().len(), 1);
}
