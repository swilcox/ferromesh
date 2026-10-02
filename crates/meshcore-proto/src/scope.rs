//! Flood scopes, which MeshCore calls regions.
//!
//! A scoped flood packet carries a transport code: the first two bytes of an
//! HMAC of the packet under the region's key. Repeaters configured for the
//! region pass it on, and others may drop it. Unscoped traffic, written `*`,
//! carries no code.
//!
//! Regions are named without `#` (`region put us-tn-middle` on a repeater),
//! but the key is derived as for a hashtag channel, from the name with `#`
//! in front (`TransportKeyStore::getAutoKeyFor` in the firmware). Packets
//! heard on the mesh confirm it: their codes match `#us` and `#nashmesh`,
//! and not the bare names.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// A region's name as shown: without the `#`, which the key adds itself.
pub fn region_name(name: &str) -> &str {
    let name = name.trim();
    name.strip_prefix('#').unwrap_or(name)
}

/// The 16-byte key for a region, named with or without `#`.
pub fn region_key(name: &str) -> [u8; 16] {
    let digest = Sha256::digest(format!("#{}", region_name(name)).as_bytes());
    digest[..16].try_into().expect("SHA-256 digest is 32 bytes")
}

/// The transport code a packet scoped to `key` carries. `payload_type` is
/// the header's 4-bit type. Codes 0 and 0xFFFF are reserved, so the firmware
/// moves them one step inward.
pub fn transport_code(key: &[u8; 16], payload_type: u8, payload: &[u8]) -> u16 {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(&[payload_type]);
    mac.update(payload);
    let digest = mac.finalize().into_bytes();
    match u16::from_le_bytes([digest[0], digest[1]]) {
        0 => 1,
        0xFFFF => 0xFFFE,
        code => code,
    }
}

/// The first of `names` whose key gives a packet this transport code.
pub fn find_region<'a>(
    names: impl IntoIterator<Item = &'a str>,
    code: u16,
    payload_type: u8,
    payload: &[u8],
) -> Option<&'a str> {
    names
        .into_iter()
        .find(|name| transport_code(&region_key(name), payload_type, payload) == code)
        .map(region_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_are_keyed_like_hashtags() {
        let key = region_key("us-tn-middle");
        assert_eq!(hex::encode(key), "69df2ce74934dc0d07b093c824d204b8");
        assert_eq!(region_key("#us-tn-middle"), key, "the # is optional");
        assert_eq!(region_name(" #us-tn "), "us-tn");
    }

    #[test]
    fn transport_codes_match_an_independent_hmac() {
        // From Python's hmac module, over the same key and payload.
        let payload: Vec<u8> = (0..40).collect();
        let code = transport_code(&region_key("us-tn-middle"), 5, &payload);
        assert_eq!(code, 0xb65e);
        let names = ["us", "us-tn", "#us-tn-middle", "nashmesh"];
        assert_eq!(find_region(names, code, 5, &payload), Some("us-tn-middle"));
        assert_eq!(find_region(names, code ^ 1, 5, &payload), None);
    }
}
