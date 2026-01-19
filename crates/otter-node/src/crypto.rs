//! Cryptographic operations compatible with Node.js crypto module.
//!
//! Provides:
//! - `randomBytes(size)` - Generate cryptographically secure random bytes
//! - `randomUUID()` - Generate a random UUID v4
//! - `createHash(algorithm)` - Create hash (md5, sha1, sha256, sha384, sha512)
//! - `createHmac(algorithm, key)` - Create HMAC
//! - `getRandomValues(typedArray)` - Web Crypto API

use md5::{Digest as Md5Digest, Md5};
use ring::digest::{self, Context as DigestContext};
use ring::hmac;
use sha1::Sha1;
use std::fmt;
use thiserror::Error;

/// Errors that can occur in crypto operations.
#[derive(Error, Debug)]
pub enum CryptoError {
    #[error("Unsupported algorithm: {0}")]
    UnsupportedAlgorithm(String),

    #[error("Invalid key length for algorithm")]
    InvalidKeyLength,

    #[error("Random generation failed: {0}")]
    RandomError(String),

    #[error("Encoding error: {0}")]
    EncodingError(String),
}

/// Supported hash algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Md5,
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlgorithm {
    /// Parse algorithm name from string.
    pub fn parse(s: &str) -> Result<Self, CryptoError> {
        match s.to_lowercase().as_str() {
            "md5" => Ok(HashAlgorithm::Md5),
            "sha1" | "sha-1" => Ok(HashAlgorithm::Sha1),
            "sha256" | "sha-256" => Ok(HashAlgorithm::Sha256),
            "sha384" | "sha-384" => Ok(HashAlgorithm::Sha384),
            "sha512" | "sha-512" => Ok(HashAlgorithm::Sha512),
            _ => Err(CryptoError::UnsupportedAlgorithm(s.to_string())),
        }
    }

    /// Get the ring digest algorithm (only for SHA-256/384/512).
    fn to_ring_algorithm(self) -> Option<&'static digest::Algorithm> {
        match self {
            HashAlgorithm::Sha256 => Some(&digest::SHA256),
            HashAlgorithm::Sha384 => Some(&digest::SHA384),
            HashAlgorithm::Sha512 => Some(&digest::SHA512),
            _ => None,
        }
    }

    /// Get the ring HMAC algorithm (only for SHA-256/384/512).
    fn to_hmac_algorithm(self) -> Option<hmac::Algorithm> {
        match self {
            HashAlgorithm::Sha256 => Some(hmac::HMAC_SHA256),
            HashAlgorithm::Sha384 => Some(hmac::HMAC_SHA384),
            HashAlgorithm::Sha512 => Some(hmac::HMAC_SHA512),
            _ => None,
        }
    }
}

impl fmt::Display for HashAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HashAlgorithm::Md5 => write!(f, "md5"),
            HashAlgorithm::Sha1 => write!(f, "sha1"),
            HashAlgorithm::Sha256 => write!(f, "sha256"),
            HashAlgorithm::Sha384 => write!(f, "sha384"),
            HashAlgorithm::Sha512 => write!(f, "sha512"),
        }
    }
}

/// Internal hash context that can use different backends.
enum HashContext {
    Md5(Md5),
    Sha1(Sha1),
    Ring(DigestContext),
}

/// A hash object for incremental hashing.
pub struct Hash {
    context: HashContext,
    algorithm: HashAlgorithm,
}

impl Hash {
    /// Create a new hash with the given algorithm.
    pub fn new(algorithm: HashAlgorithm) -> Self {
        let context = match algorithm {
            HashAlgorithm::Md5 => HashContext::Md5(Md5::new()),
            HashAlgorithm::Sha1 => HashContext::Sha1(Sha1::new()),
            HashAlgorithm::Sha256 | HashAlgorithm::Sha384 | HashAlgorithm::Sha512 => {
                HashContext::Ring(DigestContext::new(algorithm.to_ring_algorithm().unwrap()))
            }
        };
        Self { context, algorithm }
    }

    /// Update the hash with data.
    pub fn update(&mut self, data: &[u8]) {
        match &mut self.context {
            HashContext::Md5(ctx) => ctx.update(data),
            HashContext::Sha1(ctx) => ctx.update(data),
            HashContext::Ring(ctx) => ctx.update(data),
        }
    }

    /// Finalize and return the digest.
    pub fn digest(self) -> Vec<u8> {
        match self.context {
            HashContext::Md5(ctx) => ctx.finalize().to_vec(),
            HashContext::Sha1(ctx) => ctx.finalize().to_vec(),
            HashContext::Ring(ctx) => ctx.finish().as_ref().to_vec(),
        }
    }

    /// Get the algorithm name.
    pub fn algorithm(&self) -> HashAlgorithm {
        self.algorithm
    }
}

/// An HMAC object for message authentication.
pub struct Hmac {
    key: hmac::Key,
    data: Vec<u8>,
    algorithm: HashAlgorithm,
}

impl Hmac {
    /// Create a new HMAC with the given algorithm and key.
    /// Note: Only SHA-256/384/512 are supported for HMAC.
    pub fn new(algorithm: HashAlgorithm, key: &[u8]) -> Result<Self, CryptoError> {
        let hmac_alg = algorithm
            .to_hmac_algorithm()
            .ok_or_else(|| CryptoError::UnsupportedAlgorithm(format!("{} HMAC", algorithm)))?;
        Ok(Self {
            key: hmac::Key::new(hmac_alg, key),
            data: Vec::new(),
            algorithm,
        })
    }

    /// Update the HMAC with data.
    pub fn update(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data);
    }

    /// Finalize and return the HMAC digest.
    pub fn digest(self) -> Vec<u8> {
        hmac::sign(&self.key, &self.data).as_ref().to_vec()
    }

    /// Get the algorithm name.
    pub fn algorithm(&self) -> HashAlgorithm {
        self.algorithm
    }
}

/// Generate cryptographically secure random bytes.
pub fn random_bytes(size: usize) -> Result<Vec<u8>, CryptoError> {
    let mut buf = vec![0u8; size];
    getrandom::fill(&mut buf).map_err(|e| CryptoError::RandomError(e.to_string()))?;
    Ok(buf)
}

/// Generate a random UUID v4.
pub fn random_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Create a hash with the given algorithm.
pub fn create_hash(algorithm: &str) -> Result<Hash, CryptoError> {
    let alg = HashAlgorithm::parse(algorithm)?;
    Ok(Hash::new(alg))
}

/// Create an HMAC with the given algorithm and key.
pub fn create_hmac(algorithm: &str, key: &[u8]) -> Result<Hmac, CryptoError> {
    let alg = HashAlgorithm::parse(algorithm)?;
    Hmac::new(alg, key)
}

/// One-shot hash computation.
pub fn hash(algorithm: &str, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let alg = HashAlgorithm::parse(algorithm)?;
    match alg {
        HashAlgorithm::Md5 => {
            let mut hasher = Md5::new();
            hasher.update(data);
            Ok(hasher.finalize().to_vec())
        }
        HashAlgorithm::Sha1 => {
            let mut hasher = Sha1::new();
            hasher.update(data);
            Ok(hasher.finalize().to_vec())
        }
        _ => {
            let digest = digest::digest(alg.to_ring_algorithm().unwrap(), data);
            Ok(digest.as_ref().to_vec())
        }
    }
}

/// One-shot HMAC computation.
pub fn hmac_sign(algorithm: &str, key: &[u8], data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let alg = HashAlgorithm::parse(algorithm)?;
    let hmac_alg = alg
        .to_hmac_algorithm()
        .ok_or_else(|| CryptoError::UnsupportedAlgorithm(format!("{} HMAC", alg)))?;
    let hmac_key = hmac::Key::new(hmac_alg, key);
    Ok(hmac::sign(&hmac_key, data).as_ref().to_vec())
}

/// Verify an HMAC.
pub fn hmac_verify(
    algorithm: &str,
    key: &[u8],
    data: &[u8],
    signature: &[u8],
) -> Result<bool, CryptoError> {
    let alg = HashAlgorithm::parse(algorithm)?;
    let hmac_alg = alg
        .to_hmac_algorithm()
        .ok_or_else(|| CryptoError::UnsupportedAlgorithm(format!("{} HMAC", alg)))?;
    let hmac_key = hmac::Key::new(hmac_alg, key);
    Ok(hmac::verify(&hmac_key, data, signature).is_ok())
}

/// Encode bytes to hex string.
pub fn to_hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// Encode bytes to base64 string.
pub fn to_base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode hex string to bytes.
pub fn from_hex(s: &str) -> Result<Vec<u8>, CryptoError> {
    hex::decode(s).map_err(|e| CryptoError::EncodingError(e.to_string()))
}

/// Decode base64 string to bytes.
pub fn from_base64(s: &str) -> Result<Vec<u8>, CryptoError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| CryptoError::EncodingError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_bytes() {
        let bytes = random_bytes(32).unwrap();
        assert_eq!(bytes.len(), 32);

        // Two random generations should be different
        let bytes2 = random_bytes(32).unwrap();
        assert_ne!(bytes, bytes2);
    }

    #[test]
    fn test_random_uuid() {
        let uuid = random_uuid();
        assert_eq!(uuid.len(), 36); // UUID format: 8-4-4-4-12
        assert!(uuid.contains('-'));

        // Two UUIDs should be different
        let uuid2 = random_uuid();
        assert_ne!(uuid, uuid2);
    }

    #[test]
    fn test_hash_sha256() {
        let mut h = create_hash("sha256").unwrap();
        h.update(b"hello");
        let digest = h.digest();

        // Known SHA256 of "hello"
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(to_hex(&digest), expected);
    }

    #[test]
    fn test_hash_sha512() {
        let result = hash("sha512", b"hello").unwrap();
        assert_eq!(result.len(), 64); // SHA512 produces 64 bytes
    }

    #[test]
    fn test_hmac_sha256() {
        let mut h = create_hmac("sha256", b"secret").unwrap();
        h.update(b"hello");
        let mac = h.digest();

        // Verify length
        assert_eq!(mac.len(), 32); // SHA256 HMAC is 32 bytes

        // Verify against one-shot
        let mac2 = hmac_sign("sha256", b"secret", b"hello").unwrap();
        assert_eq!(mac, mac2);
    }

    #[test]
    fn test_hmac_verify() {
        let mac = hmac_sign("sha256", b"secret", b"hello").unwrap();
        assert!(hmac_verify("sha256", b"secret", b"hello", &mac).unwrap());
        assert!(!hmac_verify("sha256", b"wrong", b"hello", &mac).unwrap());
    }

    #[test]
    fn test_hex_encoding() {
        let bytes = b"hello";
        let hex_str = to_hex(bytes);
        assert_eq!(hex_str, "68656c6c6f");

        let decoded = from_hex(&hex_str).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn test_base64_encoding() {
        let bytes = b"hello";
        let b64 = to_base64(bytes);
        assert_eq!(b64, "aGVsbG8=");

        let decoded = from_base64(&b64).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn test_hash_md5() {
        let mut h = create_hash("md5").unwrap();
        h.update(b"hello");
        let digest = h.digest();

        // Known MD5 of "hello"
        let expected = "5d41402abc4b2a76b9719d911017c592";
        assert_eq!(to_hex(&digest), expected);
    }

    #[test]
    fn test_hash_sha1() {
        let mut h = create_hash("sha1").unwrap();
        h.update(b"hello");
        let digest = h.digest();

        // Known SHA1 of "hello"
        let expected = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";
        assert_eq!(to_hex(&digest), expected);
    }

    #[test]
    fn test_unsupported_algorithm() {
        let result = create_hash("blake2");
        assert!(result.is_err());
    }
}
