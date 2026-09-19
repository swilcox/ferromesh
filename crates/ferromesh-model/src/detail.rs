//! Nodes, and one packet with every reception of it.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::event::PacketEvent;

/// The most nodes `GET /api/v1/nodes` returns.
pub const MAX_NODES: usize = 5000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodesQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// `GET /api/v1/nodes`: a node as its newest validly signed advert describes
/// it, most recently heard first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Lowercase hex.
    pub pubkey: String,
    pub name: Option<String>,
    /// `chat`, `repeater`, `room-server`, `sensor`, ...
    pub role: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub first_seen_at: Timestamp,
    pub last_seen_at: Timestamp,
    /// Distinct adverts heard from it.
    pub adverts: i64,
}

/// `GET /api/v1/packets/{hash}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacketDetail {
    pub packet: PacketEvent,
    /// Oldest first.
    pub receptions: Vec<PacketReception>,
}

/// One observer hearing the packet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacketReception {
    pub observation_id: i64,
    pub observer: String,
    pub rx_at: Timestamp,
    pub snr: Option<f64>,
    pub rssi: Option<i64>,
    /// The frame exactly as this observer heard it, as hex.
    pub frame: String,
}
