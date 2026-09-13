//! Outer framing: header, optional transport codes, path, payload.
//!
//! ```text
//! header(1) [transport_codes(4)] path_len(1) path(hops × hash_size) payload(rest)
//! ```

use std::fmt;
use std::slice::ChunksExact;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::payload::Payload;
use crate::reader::Reader;

/// How a packet is routed: flooded through every repeater, or sent along a
/// known path. The transport variants carry two extra region/scope codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteType {
    TransportFlood,
    Flood,
    Direct,
    TransportDirect,
}

impl RouteType {
    const fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::TransportFlood,
            1 => Self::Flood,
            2 => Self::Direct,
            _ => Self::TransportDirect,
        }
    }

    pub const fn has_transport_codes(self) -> bool {
        matches!(self, Self::TransportFlood | Self::TransportDirect)
    }

    pub const fn is_flood(self) -> bool {
        matches!(self, Self::TransportFlood | Self::Flood)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PayloadType {
    Req,
    Response,
    TxtMsg,
    Ack,
    Advert,
    GrpTxt,
    GrpData,
    AnonReq,
    Path,
    Trace,
    Multipart,
    Control,
    RawCustom,
    /// Values 0x0C–0x0E, unassigned in current firmware.
    Reserved(u8),
}

impl PayloadType {
    pub const fn from_nibble(nibble: u8) -> Self {
        match nibble & 0x0F {
            0x00 => Self::Req,
            0x01 => Self::Response,
            0x02 => Self::TxtMsg,
            0x03 => Self::Ack,
            0x04 => Self::Advert,
            0x05 => Self::GrpTxt,
            0x06 => Self::GrpData,
            0x07 => Self::AnonReq,
            0x08 => Self::Path,
            0x09 => Self::Trace,
            0x0A => Self::Multipart,
            0x0B => Self::Control,
            0x0F => Self::RawCustom,
            other => Self::Reserved(other),
        }
    }

    pub const fn nibble(self) -> u8 {
        match self {
            Self::Req => 0x00,
            Self::Response => 0x01,
            Self::TxtMsg => 0x02,
            Self::Ack => 0x03,
            Self::Advert => 0x04,
            Self::GrpTxt => 0x05,
            Self::GrpData => 0x06,
            Self::AnonReq => 0x07,
            Self::Path => 0x08,
            Self::Trace => 0x09,
            Self::Multipart => 0x0A,
            Self::Control => 0x0B,
            Self::RawCustom => 0x0F,
            Self::Reserved(nibble) => nibble,
        }
    }

    /// The firmware's name for the type, as used by meshcoretomqtt.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Req => "REQ",
            Self::Response => "RESPONSE",
            Self::TxtMsg => "TXT_MSG",
            Self::Ack => "ACK",
            Self::Advert => "ADVERT",
            Self::GrpTxt => "GRP_TXT",
            Self::GrpData => "GRP_DATA",
            Self::AnonReq => "ANON_REQ",
            Self::Path => "PATH",
            Self::Trace => "TRACE",
            Self::Multipart => "MULTIPART",
            Self::Control => "CONTROL",
            Self::RawCustom => "RAW_CUSTOM",
            Self::Reserved(_) => "RESERVED",
        }
    }
}

/// The first byte: route type in bits 0–1, payload type in bits 2–5,
/// payload version in bits 6–7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Header(pub u8);

impl Header {
    pub const fn route_type(self) -> RouteType {
        RouteType::from_bits(self.0)
    }

    pub const fn payload_type(self) -> PayloadType {
        PayloadType::from_nibble(self.0 >> 2)
    }

    /// 0 is version 1 (1-byte node hashes, 2-byte MACs), the only one in use.
    pub const fn version(self) -> u8 {
        self.0 >> 6
    }
}

/// Hashes of the repeaters a packet has passed through, one per hop.
///
/// Each hop is a 1–3 byte prefix of the repeater's public key, so short
/// prefixes can match more than one node. TRACE packets reuse this field for
/// per-hop SNR instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Path<'a> {
    len_byte: u8,
    bytes: &'a [u8],
}

impl<'a> Path<'a> {
    /// The raw `path_len` byte: hop count in bits 0–5, hash size − 1 in bits 6–7.
    pub const fn len_byte(&self) -> u8 {
        self.len_byte
    }

    pub const fn hash_size(&self) -> usize {
        (self.len_byte >> 6) as usize + 1
    }

    pub const fn hop_count(&self) -> usize {
        (self.len_byte & 0x3F) as usize
    }

    pub const fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn hops(&self) -> ChunksExact<'a, u8> {
        self.bytes.chunks_exact(self.hash_size())
    }
}

/// A borrowed view of one received packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    header: Header,
    transport_codes: Option<[u16; 2]>,
    path: Path<'a>,
    payload: &'a [u8],
}

impl<'a> Packet<'a> {
    pub fn parse(raw: &'a [u8]) -> Result<Self> {
        let mut r = Reader::frame(raw);
        let header = Header(r.u8()?);
        let transport_codes = if header.route_type().has_transport_codes() {
            Some([r.u16_le()?, r.u16_le()?])
        } else {
            None
        };

        let len_byte = r.u8()?;
        if len_byte >> 6 == 0b11 {
            return Err(Error::ReservedPathHashSize(len_byte));
        }
        let shape = Path { len_byte, bytes: &[] };
        let path = Path { bytes: r.take(shape.hop_count() * shape.hash_size())?, ..shape };

        Ok(Self { header, transport_codes, path, payload: r.rest() })
    }

    pub const fn header(&self) -> Header {
        self.header
    }

    pub const fn route_type(&self) -> RouteType {
        self.header.route_type()
    }

    pub const fn payload_type(&self) -> PayloadType {
        self.header.payload_type()
    }

    pub const fn transport_codes(&self) -> Option<[u16; 2]> {
        self.transport_codes
    }

    pub const fn path(&self) -> Path<'a> {
        self.path
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }

    pub fn hash(&self) -> PacketHash {
        PacketHash::compute(self.payload_type(), self.path.len_byte, self.payload)
    }

    pub fn decode_payload(&self) -> Result<Payload<'a>> {
        Payload::parse(self.payload_type(), self.payload)
    }
}

/// Identifies a packet across relays and observers.
///
/// The first 8 bytes of SHA-256 over the payload type and payload, as in the
/// firmware's `Packet::calculatePacketHash`. The path is left out so every
/// relayed copy hashes the same. TRACE is the exception: its `path_len` is
/// hashed too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PacketHash(pub [u8; 8]);

impl PacketHash {
    pub fn compute(payload_type: PayloadType, path_len_byte: u8, payload: &[u8]) -> Self {
        let mut sha = Sha256::new();
        sha.update([payload_type.nibble()]);
        if payload_type == PayloadType::Trace {
            // One byte on the wire, but the firmware holds path_len in a
            // uint16_t and hashes `sizeof(path_len)`: two bytes, little-endian.
            sha.update(u16::from(path_len_byte).to_le_bytes());
        }
        sha.update(payload);
        Self(sha.finalize()[..8].try_into().expect("SHA-256 digest is 32 bytes"))
    }
}

/// Uppercase hex, matching the `hash` field meshcoretomqtt publishes.
impl fmt::Display for PacketHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02X}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_fields() {
        let header = Header(0x15);
        assert_eq!(header.route_type(), RouteType::Flood);
        assert_eq!(header.payload_type(), PayloadType::GrpTxt);
        assert_eq!(header.version(), 0);

        let header = Header(0xC3);
        assert_eq!(header.route_type(), RouteType::TransportDirect);
        assert_eq!(header.payload_type(), PayloadType::Req);
        assert_eq!(header.version(), 3);
    }

    #[test]
    fn payload_type_nibble_round_trips() {
        for nibble in 0..16 {
            assert_eq!(PayloadType::from_nibble(nibble).nibble(), nibble);
        }
    }

    #[test]
    fn path_hash_sizes() {
        for (size_code, hash_size) in [(0u8, 1usize), (1, 2), (2, 3)] {
            let mut raw = vec![0x09, (size_code << 6) | 2]; // flood TXT_MSG, two hops
            raw.extend(0..2 * hash_size as u8);
            raw.extend([0xAA, 0xBB]);

            let packet = Packet::parse(&raw).unwrap();
            let path = packet.path();
            assert_eq!(path.hash_size(), hash_size);
            assert_eq!(path.hop_count(), 2);
            assert_eq!(
                path.hops().collect::<Vec<_>>(),
                [&raw[2..2 + hash_size], &raw[2 + hash_size..2 + 2 * hash_size]]
            );
            assert_eq!(packet.payload(), &[0xAA, 0xBB]);
        }
    }

    #[test]
    fn transport_codes_precede_path() {
        let raw = [0x14, 0x34, 0x12, 0x78, 0x56, 0x01, 0xEE, 0x11, 0x22];
        let packet = Packet::parse(&raw).unwrap();
        assert_eq!(packet.route_type(), RouteType::TransportFlood);
        assert_eq!(packet.transport_codes(), Some([0x1234, 0x5678]));
        assert_eq!(packet.path().as_bytes(), &[0xEE]);
        assert_eq!(packet.payload(), &[0x11, 0x22]);
    }

    #[test]
    fn reserved_hash_size_is_rejected() {
        assert_eq!(Packet::parse(&[0x09, 0xC1, 0x00]), Err(Error::ReservedPathHashSize(0xC1)));
    }

    #[test]
    fn truncated_path_is_an_error() {
        assert_eq!(
            Packet::parse(&[0x09, 0x03, 0xAA]),
            Err(Error::Truncated { needed: 5, available: 3 })
        );
    }

    #[test]
    fn only_trace_hashes_include_path_len() {
        let payload = [1, 2, 3, 4];
        assert_eq!(
            PacketHash::compute(PayloadType::GrpTxt, 0x01, &payload),
            PacketHash::compute(PayloadType::GrpTxt, 0x05, &payload)
        );
        assert_ne!(
            PacketHash::compute(PayloadType::Trace, 0x01, &payload),
            PacketHash::compute(PayloadType::Trace, 0x05, &payload)
        );
    }

    #[test]
    fn trace_hash_matches_observer() {
        // The capture's only TRACE packet; its hash exposed the uint16 path_len.
        let raw =
            [0x26, 0x01, 0x27, 0x6A, 0xF0, 0x34, 0x24, 0x31, 0xFB, 0x3E, 0xD3, 0x01, 0xA6, 0xA6];
        assert_eq!(Packet::parse(&raw).unwrap().hash().to_string(), "2B49B3520FFB4995");
    }
}
