//! A made-up capture of a small mesh, for screenshots and demos.
//!
//!     cargo run -p ferromeshd --example demo_capture > demo.jsonl
//!     ferromeshd --config demo.toml import demo.jsonl --label demo
//!
//! Everyone in it is invented, so unlike a real capture it's safe to publish.
//! Times end at the moment it runs, so the TUI's "2m ago" reads naturally.

use ed25519_dalek::{Signer, SigningKey};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use meshcore_proto::{ChannelKey, GroupText};
use serde_json::{Value, json};

/// Channel conversations: (minutes ago, channel, sender, text). A weather
/// bot also posts to #weather every hour.
const CHAT: &[(i64, &str, &str, &str)] = &[
    (171, "#chat", "Juniper", "Morning all. Anyone else hearing the new repeater on Cedar Knob?"),
    (168, "#chat", "Otto K", "Yep, 2 hops from here and very clean"),
    (160, "#chat", "Marisol", "Getting it direct from the east side, SNR around 6"),
    (131, "#test", "Pixel", "test from the handheld, walking the greenway"),
    (129, "#test", "Pixel", "test 2, bridge underpass"),
    (97, "#chat", "Wren", "Solar node on the water tower is back up after the storm 🎉"),
    (94, "#chat", "Juniper", "Nice, that fills the gap by the river"),
    (88, "#chat", "Otto K", "Seeing it in my path list already"),
    (51, "#test", "Tomas", "first message from a new T-Deck, anyone copy?"),
    (50, "#test", "Marisol", "@[Tomas] copy, 3 hops, welcome aboard"),
    (49, "#test", "Wren", "@[Tomas] heard you too, good signal"),
    (
        38,
        "#emcomm",
        "Net Control",
        "Weekly check-in net starts at 19:00, reply with call and location",
    ),
    (33, "#chat", "Marisol", "Anyone up for mapping coverage along the ridge road Saturday?"),
    (31, "#chat", "Pixel", "I'm in, bringing the handheld and a spare battery"),
    (29, "#chat", "Juniper", "Count me in. Meet at the trailhead at 9?"),
    (14, "#emcomm", "Wren", "Wren, north side, checking in"),
    (12, "#emcomm", "Otto K", "Otto K, downtown, checking in"),
    (7, "#chat", "Tomas", "Thanks for the welcome! Range is way better than I expected"),
    (4, "#chat", "Otto K", "The Cedar Knob repeater makes a big difference"),
    (2, "#test", "Pixel", "test from the ridge road, 4 hops out"),
];

/// Repeaters, the first few of which publish what they hear over MQTT.
const REPEATERS: &[&str] = &["Cedar Knob", "Riverbend", "Mill Street", "Water Tower"];
const OBSERVERS: usize = 3;

/// Chat radios that advertise.
const CHATTERS: &[&str] = &["Juniper", "Otto K", "Marisol", "Pixel", "Wren", "Tomas"];

fn main() {
    let now = Timestamp::now().as_second();
    let mut rng = Rng(0x5eed);
    let mut lines: Vec<(i64, Value)> = Vec::new();

    // Keys whose first bytes differ, so every hop in a path names a repeater.
    let mut keys: Vec<SigningKey> = Vec::new();
    let mut seed = 1;
    while keys.len() < REPEATERS.len() + CHATTERS.len() {
        let key = key(seed);
        seed += 1;
        let first = key.verifying_key().to_bytes()[0];
        if keys.iter().all(|k| k.verifying_key().to_bytes()[0] != first) {
            keys.push(key);
        }
    }
    let prefix = |i: usize| keys[i].verifying_key().to_bytes()[0];

    // Each chat radio advertises once in the last few hours; repeaters every
    // three hours all day.
    let mut adverts = Vec::new();
    for (i, name) in CHATTERS.iter().enumerate() {
        adverts.push((now - 60 * (190 - 23 * i as i64), REPEATERS.len() + i, 0x81, *name));
    }
    for (i, name) in REPEATERS.iter().enumerate() {
        for round in 0..8 {
            adverts.push((now - 3600 * 3 * round - 600 * (i as i64 + 1), i, 0x82, *name));
        }
    }
    for (at, node, role, name) in adverts {
        let frame = advert(&keys[node], at, role, name);
        hear(&mut lines, &keys, at, &frame, &mut rng, &prefix);
    }

    let weather = (0..24).map(|hours| {
        let minutes = 60 * hours + 8;
        let hour =
            Timestamp::from_second(now - 60 * minutes).unwrap().to_zoned(TimeZone::system()).hour();
        // Warmest mid-afternoon.
        let temp = 62.0 + 9.0 * (std::f64::consts::PI * f64::from(hour - 15) / 12.0).cos();
        let text = format!(
            "{hour:02}:00 at Riverbend: {temp:.0}°F, wind S {} mph, humidity {}%",
            4 + hours % 5,
            60 + 3 * (hours % 9),
        );
        (minutes, "#weather", "Riverbend WX", text)
    });
    let chat = CHAT.iter().map(|&(m, c, s, t)| (m, c, s, t.to_owned()));
    for (minutes, channel, sender, text) in weather.chain(chat) {
        let at = now - 60 * minutes + rng.below(40);
        let message = GroupText {
            sender_timestamp: at as u32,
            txt_type: 0,
            attempt: 0,
            text: format!("{sender}: {text}").into_bytes(),
        };
        let payload = ChannelKey::from_hashtag(channel).encrypt(&message.to_plaintext());
        hear(&mut lines, &keys, at, &[&[0x15][..], &payload].concat(), &mut rng, &prefix);
    }

    // Status reports every 15 minutes for a day, counting what each observer
    // heard as a repeater's counters do.
    for observer in 0..OBSERVERS {
        let topic = topic(&keys, observer, "packets");
        let mut heard: Vec<i64> = lines
            .iter()
            .filter(|(_, line)| line["topic"] == topic.as_str())
            .map(|(at, _)| *at)
            .collect();
        heard.sort();
        for step in 0..96 {
            let at = now - 100 - 900 * (95 - step);
            let received = heard.partition_point(|&t| t <= at) as i64;
            lines.push(status(&keys, observer, at, step, received, &mut rng));
        }
    }

    lines.sort_by_key(|(at, _)| *at);
    for (_, line) in lines {
        println!("{line}");
    }
}

/// `frame` without its path length, as heard by each observer: the first
/// always, the others most of the time, each by its own route.
fn hear(
    lines: &mut Vec<(i64, Value)>,
    keys: &[SigningKey],
    at: i64,
    frame: &[u8],
    rng: &mut Rng,
    prefix: &dyn Fn(usize) -> u8,
) {
    let (header, payload) = frame.split_at(1);
    for observer in 0..OBSERVERS {
        if observer > 0 && rng.below(4) == 0 {
            continue;
        }
        // Up to two other repeaters relayed it, in some order.
        let mut others: Vec<usize> = (0..REPEATERS.len()).filter(|&r| r != observer).collect();
        let hops: Vec<u8> = (0..rng.below(3))
            .map(|_| prefix(others.remove(rng.below(others.len() as i64) as usize)))
            .collect();
        let frame = [header, &[hops.len() as u8], &hops, payload].concat();
        lines.push(packet(keys, observer, at + 1 + rng.below(3), &frame, rng));
    }
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn topic(keys: &[SigningKey], observer: usize, kind: &str) -> String {
    format!("meshcore/DEMO/{}/{kind}", hex::encode_upper(keys[observer].verifying_key().to_bytes()))
}

fn stamp(at: i64) -> String {
    Timestamp::from_second(at).unwrap().to_string()
}

fn packet(
    keys: &[SigningKey],
    observer: usize,
    at: i64,
    frame: &[u8],
    rng: &mut Rng,
) -> (i64, Value) {
    let hops = i64::from(frame[1]);
    let snr = 10.0 - 3.5 * hops as f64 - rng.below(40) as f64 / 10.0;
    let rssi = -60 - 12 * hops - rng.below(15);
    let line = json!({
        "topic": topic(keys, observer, "packets"),
        "timestamp": stamp(at),
        "origin": REPEATERS[observer],
        "raw": hex::encode_upper(frame),
        "SNR": format!("{snr:.1}"),
        "RSSI": rssi.to_string(),
    });
    (at, line)
}

fn advert(key: &SigningKey, at: i64, role: u8, name: &str) -> Vec<u8> {
    let pubkey = key.verifying_key().to_bytes();
    let timestamp = (at as u32).to_le_bytes();
    let app_data = [&[role][..], name.as_bytes()].concat();
    let signed = [&pubkey[..], &timestamp, &app_data].concat();
    [&[0x11][..], &pubkey, &timestamp, &key.sign(&signed).to_bytes(), &app_data].concat()
}

fn status(
    keys: &[SigningKey],
    observer: usize,
    at: i64,
    step: i64,
    received: i64,
    rng: &mut Rng,
) -> (i64, Value) {
    // Solar repeaters: down overnight, charging through the morning.
    let battery = 3880 - 20 * observer as i64 + 5 * (step - 60).max(-2 * (step - 60));
    let line = json!({
        "topic": topic(keys, observer, "status"),
        "timestamp": stamp(at),
        "origin": REPEATERS[observer],
        "status": "online",
        "model": "Heltec V4",
        "firmware_version": "v1.9.1",
        "radio": "910.525,62.5,7,5",
        "stats": {
            "battery_mv": battery,
            "uptime_secs": 86_400 * 12 + 900 * step,
            "noise_floor": -112 + rng.below(4),
            "tx_air_secs": 3 * received + rng.below(5),
            "rx_air_secs": 5 * received + rng.below(10),
            "packets_sent": received * 2 / 5,
            "packets_received": received,
            "recv_errors": received / 50,
            "queue_len": 0,
        },
    });
    (at, line)
}

/// A small deterministic generator, so every run tells the same story.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: i64) -> i64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as i64
    }
}
