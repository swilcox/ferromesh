//! Types shared by the ferromesh server and its clients: the events that
//! queries and streams return, the filter language that selects them, and
//! the HTTP/WebSocket wire format.

mod event;
pub mod filter;
mod wire;

pub use event::{Advert, DecodeState, Event, Kind, MessageEvent, ObservationEvent, PacketEvent};
pub use filter::{Filter, FilterError};
pub use wire::{
    DEFAULT_HISTORY_LIMIT, DEFAULT_PORT, Frame, Health, HistoryQuery, MAX_HISTORY_LIMIT,
    StreamQuery,
};
