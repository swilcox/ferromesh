//! MeshCore packet parsing, hashing, advert verification and channel decryption.
//!
//! Pure and I/O-free. [`companion`] covers the protocol between an app and a
//! companion radio. Layouts follow the firmware (`src/Packet.cpp`,
//! `src/Mesh.cpp`, `src/helpers/BaseChatMesh.cpp`) and `docs/payloads.md`.
//!
//! ```
//! use meshcore_proto::{ChannelKey, GroupText, Keyring, Packet, Payload};
//!
//! let raw = hex::decode(
//!     "154CAFB3691C628EF2C693C8D5E51503B5EC94E83E0BA4A4A9A9035E9324EA7B\
//!      58EE2A493EB86EFB58D0439F0A52B21107DC64D33789C34391E44B15EA",
//! )
//! .unwrap();
//! let packet = Packet::parse(&raw).unwrap();
//! assert_eq!(packet.hash().to_string(), "C7A9960D4B25298A");
//!
//! let mut keys = Keyring::with_public();
//! keys.add("#weather", ChannelKey::from_hashtag("#weather"));
//! let Payload::Group(group) = packet.decode_payload().unwrap() else {
//!     unreachable!("GRP_TXT packet")
//! };
//! let (channel, plaintext) = keys.decrypt(&group).unwrap();
//! let message = GroupText::parse(&plaintext).unwrap();
//! assert_eq!(channel.name, "#weather");
//! assert_eq!(message.text_lossy(), "Vista 096: Wx 28107");
//! ```

mod reader;

pub mod advert;
pub mod channel;
pub mod companion;
pub mod error;
pub mod mention;
pub mod packet;
pub mod payload;

pub use advert::{Advert, AppData, Location, NodeRole};
pub use channel::{Channel, ChannelKey, GroupText, Keyring, PUBLIC_CHANNEL_KEY, split_sender};
pub use error::{Error, KeyError, Result};
pub use packet::{Header, Packet, PacketHash, Path, PayloadType, RouteType};
pub use payload::{Addressed, AnonReq, Control, GroupPayload, Payload, Trace};
