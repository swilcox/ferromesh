//! Guessing hashtag channels. A hashtag channel's key comes from its name, so
//! trying likely names against undecrypted traffic finds channels nobody
//! configured.

use std::collections::{BTreeSet, HashMap};

use ferromesh_model::{Guess, GuessChannels, GuessReport};
use meshcore_proto::{ChannelKey, GroupText, Payload, PayloadType};
use rusqlite::Connection;

use crate::Result;

/// Recent messages scanned for hashtags people mention.
const MENTION_SCAN: i64 = 5_000;

/// Common names, tried on request. Regional names are best passed in.
pub const BUILTIN_NAMES: &[&str] = &[
    // General.
    "general",
    "chat",
    "talk",
    "lobby",
    "random",
    "offtopic",
    "social",
    "hello",
    "help",
    "info",
    "news",
    "events",
    "test",
    "testing",
    "tests",
    "bot",
    "bots",
    "ping",
    "echo",
    "mesh",
    "meshcore",
    "meshtastic",
    "lora",
    "radio",
    "ham",
    "hamradio",
    "amateur",
    "gmrs",
    "frs",
    "local",
    "regional",
    "repeaters",
    "repeater",
    "infrastructure",
    "admin",
    "sysop",
    "ops",
    "dev",
    "tech",
    "games",
    "trivia",
    "music",
    "sports",
    "outdoors",
    "hiking",
    "camping",
    "fishing",
    "hunting",
    "aviation",
    "adsb",
    "satellite",
    "sensors",
    "telemetry",
    "map",
    "mapping",
    "wardrive",
    // Weather and emergencies.
    "weather",
    "wx",
    "storm",
    "storms",
    "skywarn",
    "emergency",
    "emcomm",
    "sos",
    "alert",
    "alerts",
    "ares",
    "races",
    "cert",
    "fire",
    "ems",
    "scanner",
    "outage",
    "power",
    "prepper",
    "preppers",
    // Regions, US states and their abbreviations.
    "north",
    "south",
    "east",
    "west",
    "midwest",
    "southeast",
    "northeast",
    "southwest",
    "northwest",
    "alabama",
    "al",
    "alaska",
    "ak",
    "arizona",
    "az",
    "arkansas",
    "ar",
    "california",
    "ca",
    "colorado",
    "co",
    "connecticut",
    "ct",
    "delaware",
    "de",
    "florida",
    "fl",
    "georgia",
    "ga",
    "hawaii",
    "hi",
    "idaho",
    "id",
    "illinois",
    "il",
    "indiana",
    "in",
    "iowa",
    "ia",
    "kansas",
    "ks",
    "kentucky",
    "ky",
    "louisiana",
    "la",
    "maine",
    "me",
    "maryland",
    "md",
    "massachusetts",
    "ma",
    "michigan",
    "mi",
    "minnesota",
    "mn",
    "mississippi",
    "ms",
    "missouri",
    "mo",
    "montana",
    "mt",
    "nebraska",
    "ne",
    "nevada",
    "nv",
    "newhampshire",
    "nh",
    "newjersey",
    "nj",
    "newmexico",
    "nm",
    "newyork",
    "ny",
    "northcarolina",
    "nc",
    "northdakota",
    "nd",
    "ohio",
    "oh",
    "oklahoma",
    "ok",
    "oregon",
    "or",
    "pennsylvania",
    "pa",
    "rhodeisland",
    "ri",
    "southcarolina",
    "sc",
    "southdakota",
    "sd",
    "tennessee",
    "tn",
    "texas",
    "tx",
    "utah",
    "ut",
    "vermont",
    "vt",
    "virginia",
    "va",
    "washington",
    "wa",
    "westvirginia",
    "wv",
    "wisconsin",
    "wi",
    "wyoming",
    "wy",
];

pub(crate) fn guess_channels(conn: &Connection, request: &GuessChannels) -> Result<GuessReport> {
    let mut names = BTreeSet::new();
    for name in &request.names {
        names.extend(variants(name));
    }
    if request.builtin {
        for name in BUILTIN_NAMES {
            names.extend(variants(name));
        }
    }
    if request.mentions {
        for name in mentioned_hashtags(conn)? {
            names.extend(variants(&name));
        }
    }

    let mut candidates: HashMap<u8, Vec<(String, ChannelKey)>> = HashMap::new();
    for name in &names {
        let key = ChannelKey::from_hashtag(name);
        candidates.entry(key.hash()).or_default().push((name.clone(), key));
    }

    let mut waiting = conn.prepare(
        "SELECT payload_type, payload FROM packets WHERE decode_state = 1 AND channel_hash = ?1",
    )?;
    let mut hits = Vec::new();
    for (hash, keys) in candidates {
        let packets: Vec<(u8, Vec<u8>)> = waiting
            .query_map([hash], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if packets.is_empty() {
            continue;
        }
        for (name, key) in keys {
            let (mut opened, mut messages) = (0, 0);
            for (payload_type, payload) in &packets {
                let Ok(Payload::Group(group)) =
                    Payload::parse(PayloadType::from_nibble(*payload_type), payload)
                else {
                    continue;
                };
                let Some(plaintext) = key.decrypt(&group) else {
                    continue;
                };
                opened += 1;
                if group.kind == PayloadType::GrpTxt && reads_as_text(&plaintext) {
                    messages += 1;
                }
            }
            // A wrong key passes the two-byte MAC about once in 65,536
            // packets, so only readable text counts as proof.
            if messages > 0 {
                hits.push(Guess { name, hash, packets: opened, messages });
            }
        }
    }
    hits.sort_by(|a, b| b.packets.cmp(&a.packets).then_with(|| a.name.cmp(&b.name)));
    Ok(GuessReport { tried: names.len(), hits })
}

/// `Wx` and `#Wx` both become `#Wx` and `#wx`: the key depends on case, and
/// apps usually lowercase channel names.
fn variants(name: &str) -> Vec<String> {
    let name = name.trim();
    let tagged = if name.starts_with('#') { name.to_owned() } else { format!("#{name}") };
    if tagged.len() < 2 {
        return Vec::new();
    }
    let lower = tagged.to_lowercase();
    if lower == tagged { vec![tagged] } else { vec![tagged, lower] }
}

/// Hashtags in recent decoded messages, such as "come to #hidden-valley".
fn mentioned_hashtags(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare("SELECT body FROM messages ORDER BY id DESC LIMIT ?1")?;
    let mut tags = BTreeSet::new();
    for body in stmt.query_map([MENTION_SCAN], |row| row.get::<_, String>(0))? {
        tags.extend(hashtags_in(&body?));
    }
    Ok(tags)
}

fn hashtags_in(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || matches!(c, '#' | '-' | '_')))
        .filter_map(|word| word.strip_prefix('#'))
        .map(|tag| tag.trim_end_matches(['-', '_']))
        .filter(|tag| (1..=32).contains(&tag.chars().count()) && !tag.contains('#'))
        .map(|tag| format!("#{tag}"))
        .collect()
}

fn reads_as_text(plaintext: &[u8]) -> bool {
    GroupText::parse(plaintext).is_some_and(|message| {
        !message.text.is_empty()
            && std::str::from_utf8(&message.text).is_ok_and(|text| {
                !text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_variants() {
        assert_eq!(variants("Wx"), ["#Wx", "#wx"]);
        assert_eq!(variants("#bna-bot"), ["#bna-bot"]);
        assert!(variants(" # ").is_empty());
    }

    #[test]
    fn hashtags_in_messages() {
        assert_eq!(
            hashtags_in("Cmds: #joke, test. Try #Hidden-Valley- or #wx!"),
            ["#joke", "#Hidden-Valley", "#wx"]
        );
        assert!(hashtags_in("C# and ## and issue#4").is_empty());
    }

    #[test]
    fn readable_text() {
        let message = |text: &[u8]| {
            GroupText { sender_timestamp: 1, txt_type: 0, attempt: 0, text: text.to_vec() }
                .to_plaintext()
        };
        assert!(reads_as_text(&message(b"Bob: hi \xF0\x9F\x93\xA1")));
        assert!(!reads_as_text(&message(b"\xFF\xFE\x00garbage")));
        assert!(!reads_as_text(&message(b"bell\x07")));
        assert!(!reads_as_text(&message(b"")));
    }
}
