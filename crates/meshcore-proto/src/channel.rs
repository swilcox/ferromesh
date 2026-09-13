//! Group channels: key derivation, channel hash, MAC check and decryption.

use std::borrow::Cow;
use std::fmt;

use aes::Aes128;
use aes::cipher::{Array, BlockCipherDecrypt, KeyInit};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::KeyError;
use crate::payload::GroupPayload;

type HmacSha256 = Hmac<Sha256>;

/// The default public channel's key. It is published on purpose so every
/// client can read that channel.
pub const PUBLIC_CHANNEL_KEY: &str = "izOH6cXN6mrJ5e26oRXNcg==";

const AES_BLOCK: usize = 16;

/// A channel secret (16 or 32 bytes) and its one-byte hash.
#[derive(Clone, PartialEq, Eq)]
pub struct ChannelKey {
    /// Zero-padded to 32 bytes: the firmware keys HMAC with all 32 and AES
    /// with the first 16, whatever the secret's real length.
    secret: [u8; 32],
    len: usize,
    hash: u8,
}

impl ChannelKey {
    pub fn from_secret(secret: &[u8]) -> Result<Self, KeyError> {
        if !matches!(secret.len(), 16 | 32) {
            return Err(KeyError::Length(secret.len()));
        }
        let mut padded = [0u8; 32];
        padded[..secret.len()].copy_from_slice(secret);
        Ok(Self { secret: padded, len: secret.len(), hash: Sha256::digest(secret)[0] })
    }

    pub fn from_base64(key: &str) -> Result<Self, KeyError> {
        let secret = BASE64.decode(key.trim()).map_err(|_| KeyError::Base64)?;
        Self::from_secret(&secret)
    }

    /// Hashtag channels derive their secret from the name as typed, including
    /// the `#`, so knowing the name is enough to read them.
    pub fn from_hashtag(name: &str) -> Self {
        Self::from_secret(&Sha256::digest(name.as_bytes())[..16]).expect("16-byte secret")
    }

    pub fn public() -> Self {
        Self::from_base64(PUBLIC_CHANNEL_KEY).expect("built-in key is valid")
    }

    pub const fn hash(&self) -> u8 {
        self.hash
    }

    pub fn secret(&self) -> &[u8] {
        &self.secret[..self.len]
    }

    /// Returns the plaintext if `group` was encrypted with this key.
    ///
    /// The MAC is HMAC-SHA256 truncated to 2 bytes; the body is AES-128-ECB.
    /// The plaintext keeps its zero padding: [`GroupText::parse`] strips it.
    pub fn decrypt(&self, group: &GroupPayload<'_>) -> Option<Vec<u8>> {
        if group.channel_hash != self.hash {
            return None;
        }
        let mut mac = <HmacSha256 as hmac::KeyInit>::new_from_slice(&self.secret)
            .expect("HMAC accepts any key length");
        mac.update(group.ciphertext);
        mac.verify_truncated_left(&group.mac).ok()?;

        let key: [u8; AES_BLOCK] = self.secret[..AES_BLOCK].try_into().expect("16 bytes");
        let cipher = Aes128::new(&Array::from(key));
        // Encryption zero-pads the final block, so real ciphertext is
        // block-aligned; a stray tail can't be decrypted and is dropped.
        let mut plaintext = Vec::with_capacity(group.ciphertext.len());
        for chunk in group.ciphertext.chunks_exact(AES_BLOCK) {
            let mut block =
                aes::Block::from(<[u8; AES_BLOCK]>::try_from(chunk).expect("exact chunk"));
            cipher.decrypt_block(&mut block);
            plaintext.extend_from_slice(&block);
        }
        Some(plaintext)
    }
}

/// Shows the hash, never the secret.
impl fmt::Debug for ChannelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelKey")
            .field("hash", &format_args!("{:#04x}", self.hash))
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub name: String,
    pub key: ChannelKey,
}

/// The channels we can read.
#[derive(Debug, Clone, Default)]
pub struct Keyring {
    channels: Vec<Channel>,
}

impl Keyring {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_public() -> Self {
        let mut keyring = Self::new();
        keyring.add("public", ChannelKey::public());
        keyring
    }

    /// Adds a channel. Returns `false`, changing nothing, if a channel with
    /// the same key is already present.
    pub fn add(&mut self, name: impl Into<String>, key: ChannelKey) -> bool {
        if self.channels.iter().any(|c| c.key == key) {
            return false;
        }
        self.channels.push(Channel { name: name.into(), key });
        true
    }

    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    /// Tries each channel whose hash matches. One-byte hashes collide, so the
    /// MAC decides; the first channel that verifies wins.
    pub fn decrypt(&self, group: &GroupPayload<'_>) -> Option<(&Channel, Vec<u8>)> {
        self.channels
            .iter()
            .find_map(|channel| channel.key.decrypt(group).map(|plaintext| (channel, plaintext)))
    }
}

/// Decrypted GRP_TXT: `timestamp(4) | txt_type << 2 | attempt (1) | text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupText {
    /// The sender's clock, which may be wrong.
    pub sender_timestamp: u32,
    pub txt_type: u8,
    /// Retry counter (0–3); retries re-encrypt, so they hash differently.
    pub attempt: u8,
    /// Up to the first NUL; usually `"Sender Name: body"`.
    pub text: Vec<u8>,
}

impl GroupText {
    pub fn parse(plaintext: &[u8]) -> Option<Self> {
        let (head, body) = plaintext.split_at_checked(5)?;
        let text = body.split(|&b| b == 0).next().unwrap_or_default();
        Some(Self {
            sender_timestamp: u32::from_le_bytes(head[..4].try_into().expect("4 bytes")),
            txt_type: head[4] >> 2,
            attempt: head[4] & 0x03,
            text: text.to_vec(),
        })
    }

    pub fn text_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.text)
    }
}

/// Splits channel text into `(sender, body)` at the first `": "`.
///
/// Clients prefix messages with their display name. Nothing authenticates
/// it: any node can use any name.
pub fn split_sender(text: &str) -> (Option<&str>, &str) {
    match text.split_once(": ") {
        Some((sender, body)) => (Some(sender), body),
        None => (None, text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::PayloadType;

    #[test]
    fn public_channel_hash() {
        assert_eq!(ChannelKey::public().hash(), 0x11);
        assert_eq!(ChannelKey::public().secret().len(), 16);
    }

    #[test]
    fn secret_lengths() {
        assert_eq!(ChannelKey::from_secret(&[0; 15]), Err(KeyError::Length(15)));
        assert_eq!(ChannelKey::from_secret(&[0; 32]).unwrap().secret().len(), 32);
        assert_eq!(ChannelKey::from_base64("not base64!"), Err(KeyError::Base64));
    }

    #[test]
    fn debug_hides_the_secret() {
        assert_eq!(format!("{:?}", ChannelKey::public()), "ChannelKey { hash: 0x11, len: 16, .. }");
    }

    #[test]
    fn bad_mac_does_not_decrypt() {
        let key = ChannelKey::public();
        let group = GroupPayload {
            kind: PayloadType::GrpTxt,
            channel_hash: key.hash(),
            mac: [0, 0],
            ciphertext: &[0; 16],
        };
        assert_eq!(key.decrypt(&group), None);
    }

    #[test]
    fn keyring_ignores_duplicate_keys() {
        let mut keyring = Keyring::with_public();
        assert!(!keyring.add("also public", ChannelKey::public()));
        assert!(keyring.add("#test", ChannelKey::from_hashtag("#test")));
        assert_eq!(keyring.channels().len(), 2);
    }

    #[test]
    fn group_text_layout() {
        let mut plaintext = 0x0102_0304u32.to_le_bytes().to_vec();
        plaintext.push((1 << 2) | 2);
        plaintext.extend(b"Bob: hi\0\0\0");
        let message = GroupText::parse(&plaintext).unwrap();
        assert_eq!(message.sender_timestamp, 0x0102_0304);
        assert_eq!((message.txt_type, message.attempt), (1, 2));
        assert_eq!(message.text, b"Bob: hi");
        assert_eq!(GroupText::parse(&plaintext[..4]), None);
    }

    #[test]
    fn split_sender_at_first_separator() {
        assert_eq!(split_sender("W4X: re: hi"), (Some("W4X"), "re: hi"));
        assert_eq!(split_sender("no sender"), (None, "no sender"));
    }
}
