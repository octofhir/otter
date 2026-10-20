//! Little-endian byte writer and bounds-checked reader for startup images.
//!
//! # Contents
//! - [`ImageWriter`] — appends scalars, byte strings and strings.
//! - [`ImageReader`] — reads them back; every read is bounds-checked and a
//!   short or malformed stream answers `None`.
//!
//! # Invariants
//! - Scalars are fixed-width little-endian; byte strings and strings are a
//!   `u32` length followed by their bytes.
//! - A reader never panics on input and never allocates more than the bytes
//!   it was given hold.
//!
//! # See also
//! - [`crate::heap_image`] — the page image these encode.

/// Appends an image's fields to one byte buffer.
#[derive(Debug, Default)]
pub struct ImageWriter {
    bytes: Vec<u8>,
}

impl ImageWriter {
    /// An empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes written so far.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Append one byte.
    pub fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    /// Append a `u32`.
    pub fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a `u64`.
    pub fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a length that fits `u32`.
    ///
    /// # Panics
    /// When `len` exceeds `u32::MAX`; no image field is that long.
    pub fn len(&mut self, len: usize) {
        self.u32(u32::try_from(len).expect("image field length fits u32"));
    }

    /// Append a length-prefixed byte string.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.bytes.extend_from_slice(bytes);
    }

    /// Append a length-prefixed UTF-8 string.
    pub fn str(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    /// Append length-prefixed UTF-16 units.
    pub fn units(&mut self, units: &[u16]) {
        self.len(units.len());
        for unit in units {
            self.bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
}

/// Reads an image's fields back from a byte slice.
#[derive(Debug, Clone, Copy)]
pub struct ImageReader<'a> {
    bytes: &'a [u8],
}

impl<'a> ImageReader<'a> {
    /// A reader over `bytes`.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Whether every byte was read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The next `len` bytes.
    pub fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.bytes.len() < len {
            return None;
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Some(head)
    }

    /// One byte.
    pub fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    /// A `u32`.
    pub fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("four bytes")))
    }

    /// A `u64`.
    pub fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
    }

    /// A length, bounded by the bytes left so a corrupt count cannot ask
    /// for more elements than could follow.
    pub fn len(&mut self) -> Option<usize> {
        let len = self.u32()? as usize;
        (len <= self.bytes.len()).then_some(len)
    }

    /// A length-prefixed byte string.
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    /// A length-prefixed UTF-8 string.
    pub fn str(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.bytes()?).ok()
    }

    /// Length-prefixed UTF-16 units.
    pub fn units(&mut self) -> Option<Vec<u16>> {
        let len = self.u32()? as usize;
        let bytes = self.take(len.checked_mul(2)?)?;
        Some(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_round_trip_and_a_short_stream_reads_none() {
        let mut writer = ImageWriter::new();
        writer.u8(7);
        writer.u32(0xdead_beef);
        writer.u64(u64::MAX - 1);
        writer.str("otter");
        writer.units(&[0x61, 0xd800]);
        let bytes = writer.into_bytes();
        let mut reader = ImageReader::new(&bytes);
        assert_eq!(reader.u8(), Some(7));
        assert_eq!(reader.u32(), Some(0xdead_beef));
        assert_eq!(reader.u64(), Some(u64::MAX - 1));
        assert_eq!(reader.str(), Some("otter"));
        assert_eq!(reader.units(), Some(vec![0x61, 0xd800]));
        assert!(reader.is_empty());
        let mut short = ImageReader::new(&bytes[..bytes.len() - 1]);
        short.u8();
        short.u32();
        short.u64();
        short.str();
        assert_eq!(short.units(), None);
    }
}
