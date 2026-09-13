//! Adverts: a node's signed, cleartext "I exist" beacon. Every node directory
//! entry (name, role, position) comes from these.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::error::Result;
use crate::packet::PayloadType;
use crate::reader::Reader;

pub const PUB_KEY_SIZE: usize = 32;
pub const SIGNATURE_SIZE: usize = 64;

const FLAG_LOCATION: u8 = 0x10;
const FLAG_FEATURE1: u8 = 0x20;
const FLAG_FEATURE2: u8 = 0x40;
const FLAG_NAME: u8 = 0x80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Advert<'a> {
    pub pubkey: &'a [u8; PUB_KEY_SIZE],
    /// The sender's clock when it emitted the advert, which may be wrong.
    pub timestamp: u32,
    pub signature: &'a [u8; SIGNATURE_SIZE],
    /// Flags and optional fields; decode with [`Advert::parse_app_data`].
    pub app_data: &'a [u8],
}

impl<'a> Advert<'a> {
    pub fn parse(payload: &'a [u8]) -> Result<Self> {
        let mut r = Reader::payload(PayloadType::Advert, payload);
        Ok(Self {
            pubkey: r.array()?,
            timestamp: r.u32_le()?,
            signature: r.array()?,
            app_data: r.rest(),
        })
    }

    /// Checks the Ed25519 signature over `pubkey ‖ timestamp ‖ app_data`, the
    /// bytes the firmware signs in `Mesh::createAdvert`.
    pub fn verify(&self) -> bool {
        let Ok(key) = VerifyingKey::from_bytes(self.pubkey) else {
            return false;
        };
        let mut message = Vec::with_capacity(PUB_KEY_SIZE + 4 + self.app_data.len());
        message.extend_from_slice(self.pubkey);
        message.extend_from_slice(&self.timestamp.to_le_bytes());
        message.extend_from_slice(self.app_data);
        key.verify(&message, &Signature::from_bytes(self.signature)).is_ok()
    }

    /// The flags byte says which optional fields follow, in this order.
    pub fn parse_app_data(&self) -> Result<AppData<'a>> {
        let mut r = Reader::payload(PayloadType::Advert, self.app_data);
        let flags = r.u8()?;
        let location = if flags & FLAG_LOCATION != 0 {
            Some(Location { lat_e6: r.i32_le()?, lon_e6: r.i32_le()? })
        } else {
            None
        };
        let feature1 = if flags & FLAG_FEATURE1 != 0 { Some(r.u16_le()?) } else { None };
        let feature2 = if flags & FLAG_FEATURE2 != 0 { Some(r.u16_le()?) } else { None };
        let name = if flags & FLAG_NAME != 0 { Some(r.rest()) } else { None };
        Ok(AppData { flags, location, feature1, feature2, name })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppData<'a> {
    pub flags: u8,
    pub location: Option<Location>,
    pub feature1: Option<u16>,
    pub feature2: Option<u16>,
    /// Display name as sent: usually UTF-8, not guaranteed.
    pub name: Option<&'a [u8]>,
}

impl AppData<'_> {
    pub const fn role(&self) -> NodeRole {
        NodeRole::from_flags(self.flags)
    }
}

/// Position in millionths of a degree, as transmitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Location {
    pub lat_e6: i32,
    pub lon_e6: i32,
}

impl Location {
    pub fn lat(&self) -> f64 {
        f64::from(self.lat_e6) / 1e6
    }

    pub fn lon(&self) -> f64 {
        f64::from(self.lon_e6) / 1e6
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeRole {
    Unspecified,
    /// A companion radio: a person's chat device.
    Chat,
    Repeater,
    RoomServer,
    Sensor,
    Other(u8),
}

impl NodeRole {
    /// The role is the low nibble of the advert flags.
    pub const fn from_flags(flags: u8) -> Self {
        match flags & 0x0F {
            0 => Self::Unspecified,
            1 => Self::Chat,
            2 => Self::Repeater,
            3 => Self::RoomServer,
            4 => Self::Sensor,
            other => Self::Other(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_data_optional_fields() {
        let mut payload = vec![0u8; PUB_KEY_SIZE + 4 + SIGNATURE_SIZE];
        payload[32..36].copy_from_slice(&1_700_000_000u32.to_le_bytes());
        payload.push(FLAG_NAME | FLAG_FEATURE1 | FLAG_LOCATION | 2);
        payload.extend(36_150_000i32.to_le_bytes());
        payload.extend((-86_780_000i32).to_le_bytes());
        payload.extend(7u16.to_le_bytes());
        payload.extend(b"Tanyard");

        let advert = Advert::parse(&payload).unwrap();
        assert_eq!(advert.timestamp, 1_700_000_000);

        let app = advert.parse_app_data().unwrap();
        assert_eq!(app.role(), NodeRole::Repeater);
        assert_eq!(app.location, Some(Location { lat_e6: 36_150_000, lon_e6: -86_780_000 }));
        assert_eq!((app.feature1, app.feature2), (Some(7), None));
        assert_eq!(app.name, Some(&b"Tanyard"[..]));
    }

    #[test]
    fn missing_flags_byte_is_an_error() {
        let payload = [0u8; PUB_KEY_SIZE + 4 + SIGNATURE_SIZE];
        assert!(Advert::parse(&payload).unwrap().parse_app_data().is_err());
    }
}
