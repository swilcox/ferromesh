use crate::packet::PayloadType;

/// Why a packet or payload could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("packet truncated: needed {needed} bytes, have {available}")]
    Truncated { needed: usize, available: usize },

    #[error("{} payload truncated: needed {needed} bytes, have {available}", .kind.name())]
    PayloadTruncated { kind: PayloadType, needed: usize, available: usize },

    #[error("path length byte {0:#04x} uses the reserved hash size 0b11")]
    ReservedPathHashSize(u8),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a channel key was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("channel secret must be 16 or 32 bytes, got {0}")]
    Length(usize),

    #[error("channel key is not valid base64")]
    Base64,
}
