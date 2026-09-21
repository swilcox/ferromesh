//! Channels found or added after their traffic was stored: listing unknown
//! hashes, guessing names, and backfilling to the same database as if the
//! channel had been there from the start.

use ferromesh_model::{Backfill, Filter, GuessChannels, Kind};
use ferromesh_store::{ChannelKind, ObserverInfo, ObserverKind, Order, Page, Reception, Store};
use meshcore_proto::{ChannelKey, GroupText};

const HIDDEN: &str = "#hidden-valley";

fn frame(channel: &str, sent: u32, text: &str) -> Vec<u8> {
    let plaintext =
        GroupText { sender_timestamp: sent, txt_type: 0, attempt: 0, text: text.into() }
            .to_plaintext();
    let mut frame = vec![0x15, 0x00];
    frame.extend(ChannelKey::from_hashtag(channel).encrypt(&plaintext));
    frame
}

fn receptions(messages: &[(&str, &str)]) -> Vec<Reception> {
    let observer = ObserverInfo {
        pubkey: [7; 32],
        name: Some("Tanyard".into()),
        iata: Some("BNA".into()),
        kind: ObserverKind::Mqtt,
    };
    messages
        .iter()
        .enumerate()
        .map(|(index, (channel, text))| Reception {
            observer: observer.clone(),
            rx_at: 1_789_000_000_000_000 + index as i64 * 1_000_000,
            frame: frame(channel, index as u32, text),
            snr: Some(-3.0),
            rssi: Some(-95),
            score: None,
            direction: None,
        })
        .collect()
}

fn traffic() -> Vec<Reception> {
    receptions(&[
        ("#test", "Alice: come over to #hidden-valley"),
        (HIDDEN, "Bob: first"),
        (HIDDEN, "Carol: second"),
        ("#test", "Dave: hi"),
        (HIDDEN, "Bob: third"),
    ])
}

fn store_with(channels: &[&str], traffic: &[Reception]) -> Store {
    let mut store = Store::open_in_memory().unwrap();
    for name in channels {
        store.add_channel(name, &ChannelKey::from_hashtag(name), ChannelKind::Hashtag, 0).unwrap();
    }
    store
        .write(|batch| traffic.iter().try_for_each(|r| batch.record_reception(r).map(drop)))
        .unwrap();
    store
}

fn guess(names: &[&str], mentions: bool) -> GuessChannels {
    GuessChannels {
        names: names.iter().map(|n| (*n).to_owned()).collect(),
        builtin: false,
        mentions,
    }
}

#[test]
fn unknown_channels_group_waiting_packets() {
    let store = store_with(&["#test"], &traffic());
    let unknown = store.unknown_channels().unwrap();
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].hash, ChannelKey::from_hashtag(HIDDEN).hash());
    assert_eq!((unknown[0].packets, unknown[0].text_packets, unknown[0].data_packets), (3, 3, 0));
    assert_eq!(unknown[0].heard, 3);
    assert!(unknown[0].first_seen_at < unknown[0].last_seen_at);
}

#[test]
fn guessing_finds_mentioned_and_named_channels() {
    let store = store_with(&["#test"], &traffic());

    let from_mentions = store.guess_channels(&guess(&[], true)).unwrap();
    let hits: Vec<_> = from_mentions.hits.iter().map(|h| (h.name.as_str(), h.messages)).collect();
    assert_eq!(hits, [(HIDDEN, 3)]);

    // "#Hidden-Valley" misses; its lowercase variant is the channel.
    let named = store.guess_channels(&guess(&["Hidden-Valley"], false)).unwrap();
    assert_eq!(named.tried, 2);
    assert_eq!(named.hits.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), [HIDDEN]);

    assert!(store.guess_channels(&guess(&["#nope", "general"], false)).unwrap().hits.is_empty());
    assert!(store.guess_channels(&GuessChannels::default()).unwrap().tried > 100);
}

#[test]
fn backfill_matches_having_the_channel_from_the_start() {
    let traffic = traffic();
    let mut late = store_with(&["#test"], &traffic);
    late.track_changes();
    late.add_channel(HIDDEN, &ChannelKey::from_hashtag(HIDDEN), ChannelKind::Hashtag, 0).unwrap();
    let id = late.channels().unwrap().last().unwrap().id;

    let backfill = late.backfill_channel(id).unwrap();
    assert_eq!(backfill, Backfill { checked: 3, decrypted: 3, messages: 3 });
    assert_eq!(late.take_changes().messages.len(), 3);
    assert!(late.unknown_channels().unwrap().is_empty());
    assert_eq!(late.backfill_channel(id).unwrap(), Backfill::default());

    let early = store_with(&["#test", HIDDEN], &traffic);
    assert_eq!(late.digest().unwrap(), early.digest().unwrap());

    let everything = Page { limit: 100, order: Order::Ascending, ..Page::default() };
    let filter: Filter = format!("chan:{HIDDEN}").parse().unwrap();
    assert_eq!(late.history(Kind::Messages, &filter, &everything).unwrap().len(), 3);
    let info = late.channel_info(id).unwrap().unwrap();
    assert_eq!((info.name.as_str(), info.messages), (HIDDEN, 3));
}

#[test]
fn backfill_spans_batches() {
    let texts: Vec<String> = (0..1_205).map(|n| format!("Bot: reading {n}")).collect();
    let messages: Vec<(&str, &str)> = texts.iter().map(|text| (HIDDEN, text.as_str())).collect();
    let mut store = store_with(&[], &receptions(&messages));
    store.add_channel(HIDDEN, &ChannelKey::from_hashtag(HIDDEN), ChannelKind::Hashtag, 0).unwrap();
    let id = store.channels().unwrap().last().unwrap().id;
    assert_eq!(
        store.backfill_channel(id).unwrap(),
        Backfill { checked: 1_205, decrypted: 1_205, messages: 1_205 }
    );
}
