//! Bounds-checked little-endian cursor shared by the frame and payload parsers.

use crate::error::{Error, Result};
use crate::packet::PayloadType;

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// The payload being read, so truncation errors can name it; `None`
    /// while reading the outer frame.
    context: Option<PayloadType>,
}

impl<'a> Reader<'a> {
    pub(crate) fn frame(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0, context: None }
    }

    pub(crate) fn payload(kind: PayloadType, buf: &'a [u8]) -> Self {
        Self { buf, pos: 0, context: Some(kind) }
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let Some(end) = self.pos.checked_add(n).filter(|&end| end <= self.buf.len()) else {
            let needed = self.pos.saturating_add(n);
            let available = self.buf.len();
            return Err(match self.context {
                None => Error::Truncated { needed, available },
                Some(kind) => Error::PayloadTruncated { kind, needed, available },
            });
        };
        let bytes = &self.buf[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<&'a [u8; N]> {
        Ok(self.take(N)?.try_into().expect("take returns exactly N bytes"))
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16_le(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(*self.array()?))
    }

    pub(crate) fn u32_le(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(*self.array()?))
    }

    pub(crate) fn i32_le(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(*self.array()?))
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let bytes = &self.buf[self.pos..];
        self.pos = self.buf.len();
        bytes
    }
}
