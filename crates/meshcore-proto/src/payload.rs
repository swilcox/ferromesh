//! Per-type payload layouts.
//!
//! Adverts, ACKs and CONTROL are cleartext. Group payloads decrypt with a
//! channel key (see [`crate::channel`]). Addressed payloads need the ECDH
//! secret between the two endpoints, so they are described but left sealed.

use crate::advert::{Advert, PUB_KEY_SIZE};
use crate::error::Result;
use crate::packet::PayloadType;
use crate::reader::Reader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload<'a> {
    Addressed(Addressed<'a>),
    Ack {
        checksum: u32,
    },
    Advert(Advert<'a>),
    Group(GroupPayload<'a>),
    AnonReq(AnonReq<'a>),
    Trace(Trace<'a>),
    Control(Control<'a>),
    /// MULTIPART, RAW_CUSTOM and reserved types, kept as bytes.
    Opaque(PayloadType, &'a [u8]),
}

impl<'a> Payload<'a> {
    pub fn parse(kind: PayloadType, bytes: &'a [u8]) -> Result<Self> {
        use PayloadType as T;

        let mut r = Reader::payload(kind, bytes);
        Ok(match kind {
            T::Req | T::Response | T::TxtMsg | T::Path => Self::Addressed(Addressed {
                kind,
                dest_hash: r.u8()?,
                src_hash: r.u8()?,
                mac: *r.array()?,
                ciphertext: r.rest(),
            }),
            T::Ack => Self::Ack { checksum: r.u32_le()? },
            T::Advert => Self::Advert(Advert::parse(bytes)?),
            T::GrpTxt | T::GrpData => Self::Group(GroupPayload {
                kind,
                channel_hash: r.u8()?,
                mac: *r.array()?,
                ciphertext: r.rest(),
            }),
            T::AnonReq => Self::AnonReq(AnonReq {
                dest_hash: r.u8()?,
                sender_pubkey: r.array()?,
                mac: *r.array()?,
                ciphertext: r.rest(),
            }),
            T::Trace => Self::Trace(Trace {
                tag: r.u32_le()?,
                auth_code: r.u32_le()?,
                flags: r.u8()?,
                hashes: r.rest(),
            }),
            T::Control => Self::Control(Control { flags: r.u8()?, data: r.rest() }),
            T::Multipart | T::RawCustom | T::Reserved(_) => Self::Opaque(kind, bytes),
        })
    }
}

/// REQ, RESPONSE, TXT_MSG and PATH: one envelope between two nodes, named by
/// one-byte public-key prefixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Addressed<'a> {
    pub kind: PayloadType,
    pub dest_hash: u8,
    pub src_hash: u8,
    pub mac: [u8; 2],
    pub ciphertext: &'a [u8],
}

/// GRP_TXT or GRP_DATA on a channel. The one-byte channel hash narrows the
/// candidate keys and the MAC confirms one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupPayload<'a> {
    pub kind: PayloadType,
    pub channel_hash: u8,
    pub mac: [u8; 2],
    pub ciphertext: &'a [u8],
}

/// A request from a node the destination may not know yet, so the sender
/// includes its full public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnonReq<'a> {
    pub dest_hash: u8,
    pub sender_pubkey: &'a [u8; PUB_KEY_SIZE],
    pub mac: [u8; 2],
    pub ciphertext: &'a [u8],
}

/// A route trace. `hashes` lists the repeaters to visit (the low bits of
/// `flags` encode their size), while the packet path collects per-hop SNR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trace<'a> {
    pub tag: u32,
    pub auth_code: u32,
    pub flags: u8,
    pub hashes: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control<'a> {
    pub flags: u8,
    pub data: &'a [u8],
}

impl Control<'_> {
    pub const fn sub_type(&self) -> u8 {
        self.flags >> 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn addressed_layout() {
        let bytes = [0xAB, 0xCD, 0x01, 0x02, 0xF0, 0xF1];
        let Payload::Addressed(msg) = Payload::parse(PayloadType::TxtMsg, &bytes).unwrap() else {
            panic!("TXT_MSG should parse as Addressed");
        };
        assert_eq!(
            (msg.dest_hash, msg.src_hash, msg.mac, msg.ciphertext),
            (0xAB, 0xCD, [0x01, 0x02], &bytes[4..])
        );
    }

    #[test]
    fn truncated_payload_names_its_type() {
        assert_eq!(
            Payload::parse(PayloadType::Ack, &[1, 2]),
            Err(Error::PayloadTruncated { kind: PayloadType::Ack, needed: 4, available: 2 })
        );
    }
}
