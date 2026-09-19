//! The HTTP and WebSocket API.
//!
//! - `GET /api/v1/health` returns [`Health`].
//! - `GET /api/v1/{kind}` with a [`HistoryQuery`] returns `[Event]`, newest first.
//! - `GET /api/v1/stream` with a [`StreamQuery`] upgrades to a WebSocket of
//!   JSON [`Frame`]s: history oldest first, then [`Frame::CaughtUp`], then
//!   live events as they're stored.
//! - `GET /api/v1/channels` returns [`ChannelInfo`](crate::ChannelInfo)s, and
//!   `POST` with an [`AddChannel`](crate::AddChannel) adds one, decrypting stored
//!   traffic it opens. Changes need `Authorization: Bearer <api.token>`.
//! - `GET /api/v1/channels/unknown` returns [`UnknownChannel`](crate::UnknownChannel)s.
//! - `POST /api/v1/channels/guess` with [`GuessChannels`](crate::GuessChannels)
//!   returns a [`GuessReport`](crate::GuessReport).
//!
//! - `GET /api/v1/nodes` returns [`NodeInfo`](crate::NodeInfo)s, most recently
//!   heard first.
//! - `GET /api/v1/packets/{hash}` returns a [`PacketDetail`](crate::PacketDetail)
//!   with every reception's frame.
//! - `GET /api/v1/direct?limit=` returns
//!   [`DirectMessageInfo`](crate::DirectMessageInfo)s sent to your companion
//!   radio, newest first.
//!
//! Errors are JSON `{"error": "..."}` with a matching HTTP status.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::event::{Event, Kind};

pub const DEFAULT_PORT: u16 = 7373;
pub const DEFAULT_HISTORY_LIMIT: usize = 100;
pub const MAX_HISTORY_LIMIT: usize = 1000;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistoryQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Only ids below this, for paging back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    /// Only ids above this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
}

/// History to replay before going live is chosen in this order: everything
/// `after` an id (resuming a stream), everything `since` a time, or the `last`
/// N matches. With none of them the stream starts live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamQuery {
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<usize>,
}

// Frames are built and serialized one at a time, never stored in bulk, so the
// size difference between variants doesn't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Event {
        event: Event,
    },
    /// History is done; later events are live. Reconnect with
    /// `after=last_id` to resume without gaps or repeats.
    CaughtUp {
        last_id: i64,
    },
    /// The stream can't continue.
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MessageEvent;

    #[test]
    fn frames_are_tagged() {
        assert_eq!(
            serde_json::to_value(Frame::CaughtUp { last_id: 7 }).unwrap(),
            serde_json::json!({ "type": "caught_up", "last_id": 7 })
        );

        let frame = Frame::Event {
            event: Event::Message(MessageEvent {
                id: 3,
                packet_hash: "C7A9960D4B25298A".into(),
                first_seen_at: "2026-09-13T16:34:47Z".parse().unwrap(),
                channel: "#test".into(),
                sender: Some("Bob".into()),
                body: "hi".into(),
                sender_timestamp: 1,
                txt_type: 0,
                attempt: 0,
                heard: 2,
            }),
        };
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "event");
        assert_eq!(json["event"]["kind"], "message");
        assert_eq!(serde_json::from_value::<Frame>(json).unwrap(), frame);
    }
}
