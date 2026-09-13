//! ferromeshd: records MeshCore traffic from MQTT and serves it to clients.
//!
//! The binary in `main.rs` is a thin command line over these modules, which
//! integration tests drive directly.

pub mod api;
pub mod config;
pub mod import;
pub mod meshcoretomqtt;
pub mod pipeline;
pub mod rawlog;
pub mod rebuild;
pub mod serve;
