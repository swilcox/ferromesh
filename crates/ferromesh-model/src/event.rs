use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// What a client can query or stream. Each kind has its own id sequence,
/// increasing in the order the server stored the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Decoded channel messages, one per packet.
    Messages,
    /// Distinct packets, one per hash.
    Packets,
    /// Every reception of every packet.
    Observations,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::Messages, Self::Packets, Self::Observations];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::Packets => "packets",
            Self::Observations => "observations",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Kind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|kind| kind.as_str().eq_ignore_ascii_case(s)).ok_or_else(|| {
            format!("unknown kind {s:?}: expected messages, packets or observations")
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Event {
    Message(MessageEvent),
    Packet(PacketEvent),
    Observation(ObservationEvent),
}

impl Event {
    pub const fn kind(&self) -> Kind {
        match self {
            Self::Message(_) => Kind::Messages,
            Self::Packet(_) => Kind::Packets,
            Self::Observation(_) => Kind::Observations,
        }
    }

    pub const fn id(&self) -> i64 {
        match self {
            Self::Message(message) => message.id,
            Self::Packet(packet) => packet.id,
            Self::Observation(observation) => observation.id,
        }
    }
}

/// A decoded channel message, once per packet however many copies arrived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageEvent {
    pub id: i64,
    pub packet_hash: String,
    pub first_seen_at: Timestamp,
    pub channel: String,
    /// The name the sending radio put before `": "`. Nothing authenticates it.
    pub sender: Option<String>,
    pub body: String,
    /// The sender's clock, which may be wrong.
    pub sender_timestamp: u32,
    pub txt_type: u8,
    pub attempt: u8,
    /// Copies stored when the event was produced.
    pub heard: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacketEvent {
    pub id: i64,
    pub hash: String,
    /// Firmware name, e.g. `GRP_TXT`.
    pub payload_type: String,
    pub first_seen_at: Timestamp,
    pub last_seen_at: Timestamp,
    /// Copies stored when the event was produced.
    pub heard: i64,
    pub decode_state: DecodeState,
    /// Payload bytes.
    pub size: i64,
    pub channel: Option<String>,
    pub channel_hash: Option<u8>,
    pub advert: Option<Advert>,
    /// `"Sender: body"` for a decoded channel message.
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationEvent {
    pub id: i64,
    pub packet_id: i64,
    pub hash: String,
    pub payload_type: String,
    /// The observer's receive time.
    pub rx_at: Timestamp,
    pub observer: String,
    /// `flood`, `direct`, `transport-flood` or `transport-direct`.
    pub route: String,
    /// Repeater hash prefixes as hex, in path order (per-hop SNR for TRACE).
    pub hops: Vec<String>,
    pub snr: Option<f64>,
    pub rssi: Option<i64>,
    pub channel: Option<String>,
    pub advert_pubkey: Option<String>,
    pub advert_name: Option<String>,
    /// `"Sender: body"` for a decoded channel message.
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Advert {
    /// Lowercase hex.
    pub pubkey: String,
    pub name: Option<String>,
    /// `chat`, `repeater`, `room-server`, `sensor`, ...
    pub role: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub signature_ok: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecodeState {
    /// Nothing to decrypt: adverts, ACKs, traces.
    Cleartext,
    /// A channel packet no known key opens.
    Undecrypted,
    Decrypted,
    /// Needs an endpoint's private key: direct messages and requests.
    Sealed,
    Malformed,
}

impl DecodeState {
    /// From the integer the store keeps.
    pub const fn from_code(code: i64) -> Option<Self> {
        match code {
            0 => Some(Self::Cleartext),
            1 => Some(Self::Undecrypted),
            2 => Some(Self::Decrypted),
            3 => Some(Self::Sealed),
            4 => Some(Self::Malformed),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cleartext => "cleartext",
            Self::Undecrypted => "undecrypted",
            Self::Decrypted => "decrypted",
            Self::Sealed => "sealed",
            Self::Malformed => "malformed",
        }
    }
}
