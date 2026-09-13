//! A filter must select the same rows as SQL (history) and in memory (live
//! events). These tests run both over a small synthetic mesh, so they need no
//! captured traffic.

use ed25519_dalek::{Signer, SigningKey};
use ferromesh_model::{Event, Filter, Kind};
use ferromesh_store::{ChannelKind, ObserverInfo, Order, Page, Reception, Store};
use meshcore_proto::{ChannelKey, GroupText};

const EVERYTHING: Page = Page {
    after: None,
    before: None,
    since: None,
    until: None,
    limit: 10_000,
    order: Order::Ascending,
};

const FILTERS: &[(Kind, &str)] = &[
    (Kind::Messages, ""),
    (Kind::Messages, "chan:#test"),
    (Kind::Messages, "chan:#TEST,#wx"),
    (Kind::Messages, "-chan:#test"),
    (Kind::Messages, "from:bnabot"),
    (Kind::Messages, "from:BNA*"),
    (Kind::Messages, "from:*[x]"),
    (Kind::Messages, r#"from:"bob [x]""#),
    (Kind::Messages, "-from:bnabot"),
    (Kind::Messages, "-from:alice storm"),
    (Kind::Messages, "text:STORM chan:#wx"),
    (Kind::Packets, ""),
    (Kind::Packets, "type:advert"),
    (Kind::Packets, "type:grp_txt -chan:#test"),
    (Kind::Packets, "chan:public"),
    (Kind::Packets, "-type:advert,grp_txt"),
    (Kind::Observations, ""),
    (Kind::Observations, "observer:tanyard"),
    (Kind::Observations, "-observer:ridge"),
    (Kind::Observations, "snr>-5"),
    (Kind::Observations, "-snr>-5"),
    (Kind::Observations, "rssi<-100 type:grp_txt"),
    (Kind::Observations, "hops>1"),
    (Kind::Observations, "hops<1 chan:#test"),
    (Kind::Observations, "type:txt_msg,advert"),
];

fn observer(seed: u8, name: Option<&str>) -> ObserverInfo {
    ObserverInfo { pubkey: [seed; 32], name: name.map(Into::into), iata: Some("BNA".into()) }
}

/// A flood GRP_TXT frame with one-byte hop hashes.
fn group_frame(key: &ChannelKey, sent: u32, text: &str, path: &[u8]) -> Vec<u8> {
    let plaintext =
        GroupText { sender_timestamp: sent, txt_type: 0, attempt: 0, text: text.into() }
            .to_plaintext();
    let mut frame = vec![0x15, path.len() as u8];
    frame.extend_from_slice(path);
    frame.extend(key.encrypt(&plaintext));
    frame
}

/// A signed flood advert frame and its public key as hex.
fn advert_frame(seed: u8, timestamp: u32, flags: u8, name: &str) -> (Vec<u8>, String) {
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
    (frame, hex::encode(pubkey))
}

/// Receptions covering every filter dimension, and the repeater's public key.
fn receptions() -> (Vec<Reception>, String) {
    let tanyard = observer(1, Some("Tanyard"));
    let ridge = observer(2, Some("Ridge"));
    let unnamed = observer(3, None);
    let test = ChannelKey::from_hashtag("#test");
    let wx = ChannelKey::from_hashtag("#wx");
    let (repeater, repeater_key) = advert_frame(9, 1_789_000_000, 0x92, "Hilltop Repeater");
    let (companion, _) = advert_frame(10, 1_789_000_100, 0x81, "Chatty");
    let storm = group_frame(&test, 1, "BNABot: storm warning", &[0x11]);
    let mut storm_relayed = storm.clone();
    storm_relayed.splice(1..3, [2, 0x11, 0x22]);

    let heard = [
        (&tanyard, storm, Some(-2.5), Some(-90)),
        (&ridge, storm_relayed, Some(-8.0), Some(-110)),
        (&tanyard, group_frame(&wx, 2, "Alice: Storm is here", &[]), None, Some(-101)),
        (&unnamed, group_frame(&test, 3, "bob [x]: hello", &[0x33, 0x44, 0x55]), Some(1.0), None),
        (
            &ridge,
            group_frame(&ChannelKey::public(), 4, "no sender here", &[0x66]),
            Some(-4.0),
            Some(-95),
        ),
        (&tanyard, repeater.clone(), Some(3.0), Some(-80)),
        (&ridge, repeater, Some(-6.0), Some(-105)),
        (&unnamed, companion, None, None),
        (
            &tanyard,
            vec![0x09, 0x01, 0xAA, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC],
            Some(-1.0),
            Some(-99),
        ),
        (
            &ridge,
            group_frame(&ChannelKey::from_hashtag("#secret"), 5, "x: y", &[]),
            Some(-12.0),
            Some(-120),
        ),
    ];
    let receptions = heard
        .into_iter()
        .enumerate()
        .map(|(index, (observer, frame, snr, rssi))| Reception {
            observer: observer.clone(),
            rx_at: 1_789_000_000_000_000 + index as i64 * 1_000_000,
            frame,
            snr,
            rssi,
            score: None,
            direction: None,
        })
        .collect();
    (receptions, repeater_key)
}

fn store_with_channels() -> Store {
    let mut store = Store::open_in_memory().unwrap();
    for name in ["#test", "#wx"] {
        store.add_channel(name, &ChannelKey::from_hashtag(name), ChannelKind::Hashtag, 0).unwrap();
    }
    store
}

fn mesh() -> (Store, String) {
    let (receptions, repeater_key) = receptions();
    let mut store = store_with_channels();
    store
        .write(|batch| receptions.iter().try_for_each(|r| batch.record_reception(r).map(drop)))
        .unwrap();
    (store, repeater_key)
}

fn ids(events: &[Event]) -> Vec<i64> {
    events.iter().map(Event::id).collect()
}

#[test]
fn sql_and_memory_agree() {
    let (store, repeater_key) = mesh();
    let node = format!("node:{}", &repeater_key[..8]);
    let extra = [(Kind::Packets, node.clone()), (Kind::Observations, format!("-{node}"))];
    let filters = FILTERS.iter().map(|(kind, text)| (*kind, (*text).to_owned())).chain(extra);

    for (kind, text) in filters {
        let filter: Filter = text.parse().unwrap_or_else(|e| panic!("{text:?}: {e}"));
        filter.validate(kind).unwrap();
        let everything = store.history(kind, &Filter::default(), &EVERYTHING).unwrap();
        let in_memory: Vec<Event> = everything.into_iter().filter(|e| filter.matches(e)).collect();
        let in_sql = store.history(kind, &filter, &EVERYTHING).unwrap();
        assert_eq!(ids(&in_sql), ids(&in_memory), "{kind} {text:?}");
    }
}

#[test]
fn filters_select_what_they_say() {
    let (store, repeater_key) = mesh();
    let count = |kind: Kind, text: &str| {
        let filter: Filter = text.parse().unwrap();
        store.history(kind, &filter, &EVERYTHING).unwrap().len()
    };
    assert_eq!(count(Kind::Messages, ""), 4);
    assert_eq!(count(Kind::Messages, "chan:#test"), 2);
    assert_eq!(count(Kind::Messages, "from:bna*"), 1);
    assert_eq!(count(Kind::Messages, "storm"), 2);
    assert_eq!(count(Kind::Messages, r#"from:"bob [x]""#), 1);
    assert_eq!(count(Kind::Packets, ""), 8);
    assert_eq!(count(Kind::Packets, "type:advert"), 2);
    assert_eq!(count(Kind::Packets, &format!("node:{}", &repeater_key[..8])), 1);
    assert_eq!(count(Kind::Observations, ""), 10);
    assert_eq!(count(Kind::Observations, "observer:tanyard"), 4);
    assert_eq!(count(Kind::Observations, "snr>-5"), 5);
    assert_eq!(count(Kind::Observations, "-snr>-5"), 5);
    assert_eq!(count(Kind::Observations, "hops>1"), 2);
}

#[test]
fn events_carry_decoded_context() {
    let (store, _) = mesh();
    let messages =
        store.history(Kind::Messages, &"from:bnabot".parse().unwrap(), &EVERYTHING).unwrap();
    let [Event::Message(storm)] = messages.as_slice() else { panic!("one message: {messages:?}") };
    assert_eq!(
        (storm.channel.as_str(), storm.body.as_str(), storm.heard),
        ("#test", "storm warning", 2)
    );

    let observations = store
        .history(Kind::Observations, &"observer:ridge hops>1".parse().unwrap(), &EVERYTHING)
        .unwrap();
    let [Event::Observation(relay)] = observations.as_slice() else { panic!("{observations:?}") };
    assert_eq!(relay.hops, ["11", "22"]);
    assert_eq!(relay.route, "flood");
    assert_eq!(relay.text.as_deref(), Some("BNABot: storm warning"));

    let adverts =
        store.history(Kind::Packets, &"type:advert".parse().unwrap(), &EVERYTHING).unwrap();
    let roles: Vec<_> = adverts
        .iter()
        .map(|event| match event {
            Event::Packet(packet) => packet.advert.as_ref().and_then(|a| a.role.clone()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(roles, [Some("repeater".to_owned()), Some("chat".to_owned())]);
}

#[test]
fn paging_by_id_and_time() {
    let (store, _) = mesh();
    let all =
        |page: Page| ids(&store.history(Kind::Observations, &Filter::default(), &page).unwrap());
    assert_eq!(all(Page { limit: 3, ..Page::default() }), [10, 9, 8]);
    assert_eq!(all(Page { before: Some(8), limit: 2, ..Page::default() }), [7, 6]);
    assert_eq!(all(Page { after: Some(8), ..EVERYTHING }), [9, 10]);
    let second = 1_000_000;
    let since = Some(1_789_000_000_000_000 + 2 * second);
    let until = Some(1_789_000_000_000_000 + 4 * second);
    assert_eq!(all(Page { since, until, ..EVERYTHING }), [3, 4, 5]);
    assert_eq!(store.max_id(Kind::Observations).unwrap(), 10);
}

#[test]
fn changes_list_new_rows() {
    let (receptions, _) = receptions();
    let mut store = store_with_channels();
    store.track_changes();

    store
        .write(|batch| receptions[..2].iter().try_for_each(|r| batch.record_reception(r).map(drop)))
        .unwrap();
    let changes = store.take_changes();
    assert_eq!(
        (changes.messages.len(), changes.packets.len(), changes.observations.len()),
        (1, 1, 2)
    );
    assert!(store.take_changes().is_empty());

    let events = store.events(Kind::Observations, &changes.observations).unwrap();
    assert_eq!(ids(&events), changes.observations);
}
