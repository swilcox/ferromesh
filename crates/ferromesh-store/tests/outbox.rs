//! Messages sent through a companion radio, and how their status follows
//! what observers hear and what recipients acknowledge.

use ferromesh_model::SendStatus;
use ferromesh_store::{Acknowledgement, ObserverInfo, Reception, SentMessage, SentTo, Store};
use meshcore_proto::{ChannelKey, GroupText, Packet};

const T0: i64 = 1_789_000_000_000_000;

fn radio() -> ObserverInfo {
    ObserverInfo { pubkey: [7; 32], name: Some("desk".into()), iata: None }
}

fn tanyard() -> ObserverInfo {
    ObserverInfo { pubkey: [1; 32], name: Some("Tanyard".into()), iata: Some("BNA".into()) }
}

fn sent(to: SentTo, body: &str, sender_timestamp: u32) -> SentMessage {
    SentMessage {
        observer: radio(),
        sent_at: T0,
        to,
        body: body.into(),
        sender_timestamp,
        error: None,
    }
}

#[test]
fn channel_messages_are_heard_by_observers() {
    let text =
        GroupText { sender_timestamp: 5, txt_type: 0, attempt: 0, text: b"desk: hi".to_vec() };
    let frame =
        [&[0x15, 0x00][..], &ChannelKey::from_hashtag("#test").encrypt(&text.to_plaintext())]
            .concat();
    let packet_hash = Packet::parse(&frame).unwrap().hash().0;
    let message = sent(SentTo::Channel { name: "#test".into(), packet_hash }, "hi", 5);

    let mut store = Store::open_in_memory().unwrap();
    assert!(store.write(|batch| batch.record_sent(&message)).unwrap());
    assert!(!store.write(|batch| batch.record_sent(&message)).unwrap(), "stored once");
    let outbox = store.outbox(10, T0).unwrap();
    assert_eq!(
        (outbox[0].status, outbox[0].heard, outbox[0].to.as_str()),
        (SendStatus::Sent, 0, "#test")
    );
    assert!(!outbox[0].direct);

    let reception = Reception {
        observer: tanyard(),
        rx_at: T0 + 500_000,
        frame,
        snr: Some(10.0),
        rssi: Some(-40),
        score: None,
        direction: None,
    };
    store.write(|batch| batch.record_reception(&reception)).unwrap();
    let outbox = store.outbox(10, T0).unwrap();
    assert_eq!((outbox[0].status, outbox[0].heard), (SendStatus::Heard, 1));
    assert_eq!(outbox[0].heard_by, ["Tanyard"]);
    assert_eq!(outbox[0].packet_hash.as_deref(), Some(hex::encode_upper(packet_hash).as_str()));
}

#[test]
fn direct_messages_are_delivered_or_overdue() {
    let node = |name: Option<&str>| SentTo::Node {
        pubkey: [0xAB; 32],
        name: name.map(str::to_owned),
        expected_ack: Some(42),
        ack_timeout_ms: Some(3000),
        flood: Some(true),
    };
    // The radio returns no acknowledgement code for a message it refuses.
    let unsent = SentTo::Node {
        pubkey: [0xAB; 32],
        name: None,
        expected_ack: None,
        ack_timeout_ms: None,
        flood: None,
    };
    let mut store = Store::open_in_memory().unwrap();
    store.write(|batch| batch.record_sent(&sent(node(Some("KK4SW")), "hello", 9))).unwrap();
    let mut refused = sent(unsent, "nope", 10);
    refused.error = Some("table full".into());
    refused.sent_at = T0 - 1_000_000;
    store.write(|batch| batch.record_sent(&refused)).unwrap();

    let statuses = |store: &Store, now| -> Vec<(String, SendStatus)> {
        store.outbox(10, now).unwrap().into_iter().map(|m| (m.to, m.status)).collect()
    };
    let refused_row = ("abababababab".to_owned(), SendStatus::Failed);
    assert_eq!(
        statuses(&store, T0 + 1_000_000),
        [("KK4SW".into(), SendStatus::Sent), refused_row.clone()]
    );
    assert_eq!(
        statuses(&store, T0 + 10_000_000),
        [("KK4SW".into(), SendStatus::Unacknowledged), refused_row.clone()]
    );

    let ack =
        Acknowledgement { observer: radio(), at: T0 + 2_000_000, ack: 42, round_trip_ms: 1800 };
    assert!(store.write(|batch| batch.record_ack(&ack)).unwrap());
    assert!(!store.write(|batch| batch.record_ack(&ack)).unwrap(), "a repeat matches nothing");
    let outbox = store.outbox(10, T0 + 10_000_000).unwrap();
    assert_eq!((outbox[0].status, outbox[0].round_trip_ms), (SendStatus::Delivered, Some(1800)));
    assert!(outbox[0].direct);
    assert_eq!(store.counts().unwrap().sent_messages, 2);
}
