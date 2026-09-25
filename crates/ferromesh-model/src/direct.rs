//! Direct messages to your companion radio.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// The most direct messages `GET /api/v1/direct` returns.
pub const MAX_DIRECT: usize = 1000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// `GET /api/v1/direct`: a direct message a companion radio received and
/// decrypted, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectMessageInfo {
    pub id: i64,
    /// When ferromesh fetched it from the radio.
    pub received_at: Timestamp,
    /// The companion radio's name, or its key in hex.
    pub to: String,
    /// The sender's name, when exactly one known node has this key prefix.
    pub sender: Option<String>,
    /// The first 6 bytes of the sender's key, lowercase hex.
    pub sender_prefix: String,
    /// A room post carries its author: the first 4 bytes of their key,
    /// lowercase hex. The sender is then the room server itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_prefix: Option<String>,
    /// The author's name, when exactly one known node has that prefix. Four
    /// bytes collide more easily than six, so this stays quiet when unsure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Hops it travelled, or `None` if it came by a direct route.
    pub hops: Option<u8>,
    /// 0 plain, 1 CLI data, 2 signed room-server post.
    pub txt_type: u8,
    /// The sender's clock when it sent the message, which may be wrong.
    pub sender_timestamp: Timestamp,
    pub snr: Option<f64>,
    pub body: String,
}
