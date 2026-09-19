//! The companion radio's contact list.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// `GET /api/v1/contacts`: a node on the companion radio's contact list,
/// favourites first. The radio needs a node as a contact to exchange direct
/// messages with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadioContact {
    /// Lowercase hex.
    pub pubkey: String,
    pub name: String,
    /// `chat`, `repeater`, `room-server`, `sensor`, ...
    pub kind: String,
    /// Favourites are never replaced to make room for new contacts.
    pub favourite: bool,
    /// The node's own clock in its newest advert, which may be far off.
    pub last_advert: Option<Timestamp>,
    /// Hops on the route the radio knows, or `None` when it floods.
    pub route_hops: Option<u8>,
}

/// `POST /api/v1/contacts`, with the API token: make a node a favourite,
/// adding it to the radio if needed, or stop it being one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinRequest {
    /// A node's advertised name, or a hex prefix of its key.
    pub to: String,
    pub pinned: bool,
}
