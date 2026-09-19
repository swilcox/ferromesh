//! Types shared by the ferromesh server and its clients: the events that
//! queries and streams return, the filter language that selects them, channel
//! management, nodes, packet detail and direct messages, and the
//! HTTP/WebSocket wire format.

mod channels;
mod detail;
mod direct;
mod event;
pub mod filter;
mod wire;

pub use channels::{
    AddChannel, Backfill, ChannelAdded, ChannelInfo, Guess, GuessChannels, GuessReport,
    UnknownChannel,
};
pub use detail::{MAX_NODES, NodeInfo, NodesQuery, PacketDetail, PacketReception};
pub use direct::{DirectMessageInfo, DirectQuery, MAX_DIRECT};
pub use event::{Advert, DecodeState, Event, Kind, MessageEvent, ObservationEvent, PacketEvent};
pub use filter::{Filter, FilterError};
pub use wire::{
    DEFAULT_HISTORY_LIMIT, DEFAULT_PORT, Frame, Health, HistoryQuery, MAX_HISTORY_LIMIT,
    StreamQuery,
};
