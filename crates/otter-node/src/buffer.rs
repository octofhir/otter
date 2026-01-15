//! node:buffer implementation
//!
//! Buffer class for binary data manipulation compatible with Node.js.

use base64::{Engine as _, engine::general_purpose};

/// Buffer for binary data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer {
    data: Vec<u8>,
}

impl Buffer {
    /// Create a new buffer from bytes.
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    /// Allocate a buffer of the given size, filled with the specified byte.
    pub fn alloc(size: usize, fill: u8) -> Self {
        Self {
            data: vec![fill; size],
        }
    }

    /// Create a buffer from a string with the specified encoding.
    pub fn from_string(s: &str, encoding: &str) -> Result<Self, BufferError> {
        let data = match encoding {
            "base64" => general_purpose::STANDARD
                .decode(s)
                .map_err(|e| BufferError::InvalidEncoding(format!("Invalid base64: {}", e)))?,
            "hex" => hex::decode(s)
                .map_err(|e| BufferError::InvalidEncoding(format!("Invalid hex: {}", e)))?,
            _ => s.as_bytes().to_vec(), // Default to utf-8
        };
        Ok(Self { data })
    }

    /// Create a buffer from a byte array.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            data: bytes.to_vec(),
        }
    }

    /// Concatenate multiple buffers.
    pub fn concat(buffers: &[&Buffer], total_length: Option<usize>) -> Self {
        let mut result = Vec::new();
        for buf in buffers {
            result.extend_from_slice(&buf.data);
        }
        if let Some(len) = total_length {
            result.truncate(len);
        }
        Self { data: result }
    }

    /// Get buffer length.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Get byte length of a string with the given encoding.
    pub fn byte_length(s: &str, encoding: &str) -> usize {
        match encoding {
            "base64" => s.len() * 3 / 4,
            "hex" => s.len() / 2,
            _ => s.len(), // Default to utf-8
        }
    }

    /// Convert buffer to string with the specified encoding.
    pub fn to_string(&self, encoding: &str, start: usize, end: usize) -> String {
        let end = end.min(self.data.len());
        let start = start.min(end);
        let bytes = &self.data[start..end];

        match encoding {
            "base64" => general_purpose::STANDARD.encode(bytes),
            "hex" => hex::encode(bytes),
            _ => String::from_utf8_lossy(bytes).to_string(), // Default to utf-8
        }
    }

    /// Get a slice of the buffer.
    pub fn slice(&self, start: isize, end: isize) -> Self {
        let len = self.data.len() as isize;
        let start = if start < 0 {
            (len + start).max(0) as usize
        } else {
            start as usize
        };
        let end = if end < 0 {
            (len + end).max(0) as usize
        } else {
            (end as usize).min(self.data.len())
        };

        Self {
            data: self.data[start..end].to_vec(),
        }
    }

    /// Copy bytes from source to target.
    pub fn copy_to(
        &self,
        target: &mut Buffer,
        target_start: usize,
        source_start: usize,
        source_end: usize,
    ) -> usize {
        let source_end = source_end.min(self.data.len());
        let source_start = source_start.min(source_end);
        let bytes_to_copy =
            (source_end - source_start).min(target.data.len().saturating_sub(target_start));

        for i in 0..bytes_to_copy {
            if target_start + i < target.data.len() {
                target.data[target_start + i] = self.data[source_start + i];
            }
        }

        bytes_to_copy
    }

    /// Check if two buffers are equal.
    pub fn equals(&self, other: &Buffer) -> bool {
        self.data == other.data
    }

    /// Compare two buffers.
    pub fn compare(&self, other: &Buffer) -> i32 {
        match self.data.cmp(&other.data) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    }

    /// Get underlying bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Get mutable underlying bytes.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Get byte at index.
    pub fn get(&self, index: usize) -> Option<u8> {
        self.data.get(index).copied()
    }

    /// Set byte at index.
    pub fn set(&mut self, index: usize, value: u8) -> bool {
        if index < self.data.len() {
            self.data[index] = value;
            true
        } else {
            false
        }
    }
}

impl std::ops::Index<usize> for Buffer {
    type Output = u8;

    fn index(&self, index: usize) -> &Self::Output {
        &self.data[index]
    }
}

impl std::ops::IndexMut<usize> for Buffer {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.data[index]
    }
}

impl IntoIterator for Buffer {
    type Item = u8;
    type IntoIter = std::vec::IntoIter<u8>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.into_iter()
    }
}

impl<'a> IntoIterator for &'a Buffer {
    type Item = &'a u8;
    type IntoIter = std::slice::Iter<'a, u8>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

/// Buffer errors.
#[derive(Debug, Clone)]
pub enum BufferError {
    InvalidEncoding(String),
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferError::InvalidEncoding(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for BufferError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alloc() {
        let buf = Buffer::alloc(5, 0);
        assert_eq!(buf.len(), 5);
        assert_eq!(buf.as_bytes(), &[0, 0, 0, 0, 0]);

        let buf = Buffer::alloc(3, 42);
        assert_eq!(buf.as_bytes(), &[42, 42, 42]);
    }

    #[test]
    fn test_from_string_utf8() {
        let buf = Buffer::from_string("hello", "utf8").unwrap();
        assert_eq!(buf.as_bytes(), b"hello");
    }

    #[test]
    fn test_from_string_base64() {
        let buf = Buffer::from_string("aGVsbG8=", "base64").unwrap();
        assert_eq!(buf.as_bytes(), b"hello");
    }

    #[test]
    fn test_from_string_hex() {
        let buf = Buffer::from_string("68656c6c6f", "hex").unwrap();
        assert_eq!(buf.as_bytes(), b"hello");
    }

    #[test]
    fn test_to_string_utf8() {
        let buf = Buffer::from_bytes(b"hello");
        assert_eq!(buf.to_string("utf8", 0, buf.len()), "hello");
    }

    #[test]
    fn test_to_string_base64() {
        let buf = Buffer::from_bytes(b"hello");
        assert_eq!(buf.to_string("base64", 0, buf.len()), "aGVsbG8=");
    }

    #[test]
    fn test_to_string_hex() {
        let buf = Buffer::from_bytes(b"hello");
        assert_eq!(buf.to_string("hex", 0, buf.len()), "68656c6c6f");
    }

    #[test]
    fn test_concat() {
        let buf1 = Buffer::from_bytes(b"hello");
        let buf2 = Buffer::from_bytes(b" ");
        let buf3 = Buffer::from_bytes(b"world");

        let combined = Buffer::concat(&[&buf1, &buf2, &buf3], None);
        assert_eq!(combined.as_bytes(), b"hello world");

        let truncated = Buffer::concat(&[&buf1, &buf2, &buf3], Some(5));
        assert_eq!(truncated.as_bytes(), b"hello");
    }

    #[test]
    fn test_slice() {
        let buf = Buffer::from_bytes(b"hello world");
        let slice = buf.slice(0, 5);
        assert_eq!(slice.as_bytes(), b"hello");

        let slice = buf.slice(-5, 100);
        assert_eq!(slice.as_bytes(), b"world");
    }

    #[test]
    fn test_equals() {
        let buf1 = Buffer::from_bytes(b"hello");
        let buf2 = Buffer::from_bytes(b"hello");
        let buf3 = Buffer::from_bytes(b"world");

        assert!(buf1.equals(&buf2));
        assert!(!buf1.equals(&buf3));
    }

    #[test]
    fn test_compare() {
        let buf1 = Buffer::from_bytes(b"abc");
        let buf2 = Buffer::from_bytes(b"abc");
        let buf3 = Buffer::from_bytes(b"abd");
        let buf4 = Buffer::from_bytes(b"abb");

        assert_eq!(buf1.compare(&buf2), 0);
        assert_eq!(buf1.compare(&buf3), -1);
        assert_eq!(buf1.compare(&buf4), 1);
    }

    #[test]
    fn test_byte_length() {
        assert_eq!(Buffer::byte_length("hello", "utf8"), 5);
        assert_eq!(Buffer::byte_length("aGVsbG8=", "base64"), 6); // Approximate
        assert_eq!(Buffer::byte_length("68656c6c6f", "hex"), 5);
    }

    #[test]
    fn test_index() {
        let mut buf = Buffer::from_bytes(b"hello");
        assert_eq!(buf[0], b'h');
        buf[0] = b'H';
        assert_eq!(buf[0], b'H');
    }

    #[test]
    fn test_copy() {
        let src = Buffer::from_bytes(b"hello");
        let mut dst = Buffer::alloc(10, 0);

        let copied = src.copy_to(&mut dst, 0, 0, src.len());
        assert_eq!(copied, 5);
        assert_eq!(&dst.as_bytes()[..5], b"hello");
    }
}
