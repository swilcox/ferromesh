//! Sending through the companion radio, and the outbox that tracks what was
//! sent.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// The most outbox entries `GET /api/v1/outbox` returns.
pub const MAX_OUTBOX: usize = 1000;

/// `POST /api/v1/send`, with the API token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendRequest {
    /// A channel (`#test`, or a private channel's name), or a node for a
    /// direct message: its advertised name or a prefix of its key in hex.
    pub to: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SendStatus {
    /// The radio refused it, or couldn't be reached.
    Failed,
    /// Transmitted, with nothing heard back yet.
    Sent,
    /// A channel message whose packet an observer has heard.
    Heard,
    /// A direct message the recipient acknowledged.
    Delivered,
    /// A direct message whose acknowledgement is overdue.
    Unacknowledged,
}

impl SendStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Sent => "sent",
            Self::Heard => "heard",
            Self::Delivered => "delivered",
            Self::Unacknowledged => "unacknowledged",
        }
    }
}

/// `GET /api/v1/outbox` and `POST /api/v1/send`: something sent through the
/// companion radio, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SentMessageInfo {
    pub id: i64,
    pub sent_at: Timestamp,
    /// The companion radio that sent it.
    pub from: String,
    /// The channel, or the recipient's name (or key prefix if unnamed).
    pub to: String,
    pub direct: bool,
    pub body: String,
    /// The time inside the message, which recipients see.
    pub sender_timestamp: Timestamp,
    pub status: SendStatus,
    /// Why it failed.
    pub error: Option<String>,
    /// Direct messages: how long the acknowledgement took.
    pub round_trip_ms: Option<u32>,
    /// Channel messages: receptions of the packet, by every observer.
    pub heard: i64,
    /// Channel messages: the observers that heard the packet.
    pub heard_by: Vec<String>,
    /// Channel messages: the packet's hash, for `GET /api/v1/packets/{hash}`.
    pub packet_hash: Option<String>,
}
