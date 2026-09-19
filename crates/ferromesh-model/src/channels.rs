//! Channel management over the API. Channel secrets never leave the server.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// `GET /api/v1/channels`: a channel the server decrypts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub name: String,
    /// `public`, `hashtag` or `key`.
    pub kind: String,
    /// The one-byte hash packets on this channel carry.
    pub hash: u8,
    pub enabled: bool,
    pub added_at: Timestamp,
    pub messages: i64,
    pub last_message_at: Option<Timestamp>,
}

/// `POST /api/v1/channels`. A hashtag channel (`#name`) needs only its name;
/// any other channel needs its key, in hex or base64.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddChannel {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAdded {
    pub channel: ChannelInfo,
    pub backfill: Backfill,
}

/// What adding a channel did to traffic stored before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Backfill {
    /// Undecrypted packets carrying the channel's hash.
    pub checked: i64,
    /// Of those, the ones its key opened.
    pub decrypted: i64,
    /// Messages recorded from them.
    pub messages: i64,
}

/// `GET /api/v1/channels/unknown`: a channel hash on stored packets that no
/// known key opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownChannel {
    pub hash: u8,
    pub packets: i64,
    /// Receptions of those packets, relayed copies included.
    pub heard: i64,
    /// GRP_TXT packets.
    pub text_packets: i64,
    /// GRP_DATA packets.
    pub data_packets: i64,
    pub first_seen_at: Timestamp,
    pub last_seen_at: Timestamp,
    /// A known channel whose one-byte hash is the same, though its key doesn't
    /// open these packets.
    pub shares_hash_with: Option<String>,
}

/// `POST /api/v1/channels/guess`: hashtag names to try against undecrypted
/// traffic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuessChannels {
    /// Names to try, with or without `#`.
    #[serde(default)]
    pub names: Vec<String>,
    /// Also try the server's list of common names.
    #[serde(default = "yes")]
    pub builtin: bool,
    /// Also try hashtags mentioned in decoded messages.
    #[serde(default = "yes")]
    pub mentions: bool,
}

impl Default for GuessChannels {
    fn default() -> Self {
        Self { names: Vec::new(), builtin: true, mentions: true }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuessReport {
    /// Distinct names tried.
    pub tried: usize,
    /// Most packets first.
    pub hits: Vec<Guess>,
}

/// A name whose key opens stored traffic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guess {
    pub name: String,
    pub hash: u8,
    /// Undecrypted packets the key opens.
    pub packets: i64,
    /// Of those, the text messages that decrypt to readable text.
    pub messages: i64,
}
