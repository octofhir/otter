//! Cryptographic operations compatible with Node.js crypto module.
//!
//! Provides:
//! - `randomBytes(size)` - Generate cryptographically secure random bytes
//! - `randomUUID()` - Generate a random UUID v4
//! - `createHash(algorithm)` - Create hash (md5, sha1, sha256, sha384, sha512)
//! - `createHmac(algorithm, key)` - Create HMAC
//! - `getRandomValues(typedArray)` - Web Crypto API

use aes::Aes128;
use aes::Aes192;
use aes::Aes256;
use aes_kw::Kek;
use aes_gcm::{AeadInPlace, Aes128Gcm, Aes256Gcm, KeyInit as AesGcmKeyInit};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chacha20poly1305::ChaCha20Poly1305;
use cipher::block_padding::{NoPadding, Pkcs7};
use cipher::{BlockDecryptMut, BlockEncrypt, BlockEncryptMut, KeyIvInit, StreamCipher};
use ctr::Ctr128BE;
use ed25519_dalek::{Signer as Ed25519Signer, SigningKey as Ed25519SigningKey, VerifyingKey as Ed25519VerifyingKey};
use hkdf::Hkdf;
use md5::{Digest as Md5Digest, Md5};
use pbkdf2::pbkdf2_hmac;
use pem::Pem;
use rand_core::OsRng;
use rand_core::RngCore;
use rsa::pkcs1::{
    DecodeRsaPrivateKey, DecodeRsaPublicKey, EncodeRsaPrivateKey, EncodeRsaPublicKey,
};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use rsa::{Oaep, Pkcs1v15Sign, Pss, RsaPrivateKey, RsaPublicKey};
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use ring::digest::{self, Context as DigestContext};
use ring::hmac;
use serde::{Deserialize, Serialize};
use scrypt::{Params as ScryptParams, scrypt as scrypt_kdf};
use sha1::Sha1;
use sha2::{Sha256, Sha384, Sha512};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature as P256Signature, SigningKey as P256SigningKey, VerifyingKey as P256VerifyingKey};
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{EncodedPoint as P256EncodedPoint, FieldBytes as P256FieldBytes, PublicKey as P256PublicKey, SecretKey as P256SecretKey};
use std::fmt;
use thiserror::Error;

/// Errors that can occur in crypto operations.
#[derive(Error, Debug)]
pub enum CryptoError {
    #[error("Unsupported algorithm: {0}")]
    UnsupportedAlgorithm(String),

    #[error("Invalid key length for algorithm")]
    InvalidKeyLength,

    #[error("Invalid parameters: {0}")]
    InvalidParams(String),

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

/// Supported cipher algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CipherAlgorithm {
    Aes128Ctr,
    Aes256Ctr,
    Aes128Cbc,
    Aes256Cbc,
    Aes128Gcm,
    Aes256Gcm,
    ChaCha20Poly1305,
}

impl CipherAlgorithm {
    pub fn parse(name: &str) -> Result<Self, CryptoError> {
        match name.to_lowercase().as_str() {
            "aes-128-ctr" => Ok(Self::Aes128Ctr),
            "aes-256-ctr" => Ok(Self::Aes256Ctr),
            "aes-128-cbc" => Ok(Self::Aes128Cbc),
            "aes-256-cbc" => Ok(Self::Aes256Cbc),
            "aes-128-gcm" => Ok(Self::Aes128Gcm),
            "aes-256-gcm" => Ok(Self::Aes256Gcm),
            "chacha20-poly1305" => Ok(Self::ChaCha20Poly1305),
            _ => Err(CryptoError::UnsupportedAlgorithm(name.to_string())),
        }
    }

    pub fn key_len(self) -> usize {
        match self {
            Self::Aes128Ctr | Self::Aes128Cbc | Self::Aes128Gcm => 16,
            Self::Aes256Ctr | Self::Aes256Cbc | Self::Aes256Gcm => 32,
            Self::ChaCha20Poly1305 => 32,
        }
    }

    pub fn iv_len(self) -> usize {
        match self {
            Self::Aes128Ctr | Self::Aes256Ctr | Self::Aes128Cbc | Self::Aes256Cbc => 16,
            Self::Aes128Gcm | Self::Aes256Gcm | Self::ChaCha20Poly1305 => 12,
        }
    }

    pub fn is_aead(self) -> bool {
        matches!(
            self,
            Self::Aes128Gcm | Self::Aes256Gcm | Self::ChaCha20Poly1305
        )
    }
}

enum CipherState {
    Aes128Ctr(Ctr128BE<Aes128>),
    Aes256Ctr(Ctr128BE<Aes256>),
}

/// Cipher context for createCipheriv/createDecipheriv.
pub struct CipherContext {
    algorithm: CipherAlgorithm,
    encrypt: bool,
    auto_padding: bool,
    aad: Vec<u8>,
    buffer: Vec<u8>,
    auth_tag: Option<Vec<u8>>,
    auth_tag_len: usize,
    state: Option<CipherState>,
    key: Vec<u8>,
    iv: Vec<u8>,
}

impl CipherContext {
    pub fn new(
        algorithm: CipherAlgorithm,
        key: &[u8],
        iv: &[u8],
        encrypt: bool,
        auth_tag_len: Option<usize>,
    ) -> Result<Self, CryptoError> {
        if key.len() != algorithm.key_len() {
            return Err(CryptoError::InvalidKeyLength);
        }
        if iv.len() != algorithm.iv_len() {
            return Err(CryptoError::InvalidKeyLength);
        }
        let tag_len = auth_tag_len.unwrap_or(16);
        if algorithm.is_aead() && tag_len != 16 {
            return Err(CryptoError::InvalidKeyLength);
        }

        let state = match algorithm {
            CipherAlgorithm::Aes128Ctr => {
                Some(CipherState::Aes128Ctr(Ctr128BE::new(
                    key.into(),
                    iv.into(),
                )))
            }
            CipherAlgorithm::Aes256Ctr => {
                Some(CipherState::Aes256Ctr(Ctr128BE::new(
                    key.into(),
                    iv.into(),
                )))
            }
            _ => None,
        };

        Ok(Self {
            algorithm,
            encrypt,
            auto_padding: true,
            aad: Vec::new(),
            buffer: Vec::new(),
            auth_tag: None,
            auth_tag_len: tag_len,
            state,
            key: key.to_vec(),
            iv: iv.to_vec(),
        })
    }

    pub fn set_auto_padding(&mut self, value: bool) -> Result<(), CryptoError> {
        self.auto_padding = value;
        Ok(())
    }

    pub fn set_aad(&mut self, aad: &[u8]) -> Result<(), CryptoError> {
        if !self.algorithm.is_aead() {
            return Err(CryptoError::UnsupportedAlgorithm(format!(
                "{} does not support AAD",
                self.algorithm_name()
            )));
        }
        self.aad = aad.to_vec();
        Ok(())
    }

    pub fn set_auth_tag(&mut self, tag: &[u8]) -> Result<(), CryptoError> {
        if !self.algorithm.is_aead() {
            return Err(CryptoError::UnsupportedAlgorithm(format!(
                "{} does not support auth tag",
                self.algorithm_name()
            )));
        }
        if tag.len() != self.auth_tag_len {
            return Err(CryptoError::InvalidKeyLength);
        }
        self.auth_tag = Some(tag.to_vec());
        Ok(())
    }

    pub fn get_auth_tag(&self) -> Option<Vec<u8>> {
        self.auth_tag.clone()
    }

    pub fn update(&mut self, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if let Some(state) = &mut self.state {
            let mut out = data.to_vec();
            match state {
                CipherState::Aes128Ctr(cipher) => cipher.apply_keystream(&mut out),
                CipherState::Aes256Ctr(cipher) => cipher.apply_keystream(&mut out),
            }
            return Ok(out);
        }

        self.buffer.extend_from_slice(data);
        Ok(Vec::new())
    }

    pub fn finalize(&mut self) -> Result<Vec<u8>, CryptoError> {
        match self.algorithm {
            CipherAlgorithm::Aes128Ctr | CipherAlgorithm::Aes256Ctr => Ok(Vec::new()),
            CipherAlgorithm::Aes128Cbc => self.finalize_cbc_aes128(),
            CipherAlgorithm::Aes256Cbc => self.finalize_cbc_aes256(),
            CipherAlgorithm::Aes128Gcm => self.finalize_gcm::<Aes128Gcm>(),
            CipherAlgorithm::Aes256Gcm => self.finalize_gcm::<Aes256Gcm>(),
            CipherAlgorithm::ChaCha20Poly1305 => self.finalize_chacha20(),
        }
    }

    fn finalize_cbc_aes128(&mut self) -> Result<Vec<u8>, CryptoError> {
        let data = std::mem::take(&mut self.buffer);
        let mut buf = data;
        let block_size = 16;
        if self.encrypt {
            if self.auto_padding {
                let msg_len = buf.len();
                buf.resize(msg_len + block_size, 0);
                let encryptor = cbc::Encryptor::<Aes128>::new_from_slices(&self.key, &self.iv)
                    .map_err(|_| CryptoError::InvalidKeyLength)?;
                let result = encryptor
                    .encrypt_padded_mut::<Pkcs7>(&mut buf, msg_len)
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
                Ok(result.to_vec())
            } else {
                let msg_len = buf.len();
                buf.resize(msg_len + block_size, 0);
                let encryptor = cbc::Encryptor::<Aes128>::new_from_slices(&self.key, &self.iv)
                    .map_err(|_| CryptoError::InvalidKeyLength)?;
                let result = encryptor
                    .encrypt_padded_mut::<NoPadding>(&mut buf, msg_len)
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
                Ok(result.to_vec())
            }
        } else if self.auto_padding {
            let decryptor = cbc::Decryptor::<Aes128>::new_from_slices(&self.key, &self.iv)
                .map_err(|_| CryptoError::InvalidKeyLength)?;
            let result = decryptor
                .decrypt_padded_mut::<Pkcs7>(&mut buf)
                .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
            Ok(result.to_vec())
        } else {
            let decryptor = cbc::Decryptor::<Aes128>::new_from_slices(&self.key, &self.iv)
                .map_err(|_| CryptoError::InvalidKeyLength)?;
            let result = decryptor
                .decrypt_padded_mut::<NoPadding>(&mut buf)
                .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
            Ok(result.to_vec())
        }
    }

    fn finalize_cbc_aes256(&mut self) -> Result<Vec<u8>, CryptoError> {
        let data = std::mem::take(&mut self.buffer);
        let mut buf = data;
        let block_size = 16;
        if self.encrypt {
            if self.auto_padding {
                let msg_len = buf.len();
                buf.resize(msg_len + block_size, 0);
                let encryptor = cbc::Encryptor::<Aes256>::new_from_slices(&self.key, &self.iv)
                    .map_err(|_| CryptoError::InvalidKeyLength)?;
                let result = encryptor
                    .encrypt_padded_mut::<Pkcs7>(&mut buf, msg_len)
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
                Ok(result.to_vec())
            } else {
                let msg_len = buf.len();
                buf.resize(msg_len + block_size, 0);
                let encryptor = cbc::Encryptor::<Aes256>::new_from_slices(&self.key, &self.iv)
                    .map_err(|_| CryptoError::InvalidKeyLength)?;
                let result = encryptor
                    .encrypt_padded_mut::<NoPadding>(&mut buf, msg_len)
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
                Ok(result.to_vec())
            }
        } else if self.auto_padding {
            let decryptor = cbc::Decryptor::<Aes256>::new_from_slices(&self.key, &self.iv)
                .map_err(|_| CryptoError::InvalidKeyLength)?;
            let result = decryptor
                .decrypt_padded_mut::<Pkcs7>(&mut buf)
                .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
            Ok(result.to_vec())
        } else {
            let decryptor = cbc::Decryptor::<Aes256>::new_from_slices(&self.key, &self.iv)
                .map_err(|_| CryptoError::InvalidKeyLength)?;
            let result = decryptor
                .decrypt_padded_mut::<NoPadding>(&mut buf)
                .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
            Ok(result.to_vec())
        }
    }

    fn finalize_gcm<C>(&mut self) -> Result<Vec<u8>, CryptoError>
    where
        C: AeadInPlace + AesGcmKeyInit,
    {
        if self.auth_tag_len != 16 {
            return Err(CryptoError::InvalidKeyLength);
        }

        let key = C::new_from_slice(&self.key).unwrap();
        let nonce = aes_gcm::Nonce::from_slice(&self.iv);
        let mut buffer = std::mem::take(&mut self.buffer);
        if self.encrypt {
            let tag = key
                .encrypt_in_place_detached(nonce, self.aad.as_slice(), &mut buffer)
                .map_err(|e| CryptoError::EncodingError(format!("{e:?}")))?;
            self.auth_tag = Some(tag.to_vec());
            Ok(buffer)
        } else {
            let tag = self
                .auth_tag
                .clone()
                .ok_or_else(|| CryptoError::InvalidKeyLength)?;
            let tag = aes_gcm::Tag::from_slice(&tag);
            key.decrypt_in_place_detached(nonce, self.aad.as_slice(), &mut buffer, tag)
            .map_err(|e| CryptoError::EncodingError(format!("{e:?}")))?;
            Ok(buffer)
        }
    }

    fn finalize_chacha20(&mut self) -> Result<Vec<u8>, CryptoError> {
        if self.auth_tag_len != 16 {
            return Err(CryptoError::InvalidKeyLength);
        }

        let key = ChaCha20Poly1305::new_from_slice(&self.key)
            .map_err(|_| CryptoError::InvalidKeyLength)?;
        let nonce = chacha20poly1305::Nonce::from_slice(&self.iv);
        let mut buffer = std::mem::take(&mut self.buffer);
        if self.encrypt {
            let tag = key
                .encrypt_in_place_detached(nonce, self.aad.as_slice(), &mut buffer)
                .map_err(|e| CryptoError::EncodingError(format!("{e:?}")))?;
            self.auth_tag = Some(tag.to_vec());
            Ok(buffer)
        } else {
            let tag = self
                .auth_tag
                .clone()
                .ok_or_else(|| CryptoError::InvalidKeyLength)?;
            let tag = chacha20poly1305::Tag::from_slice(&tag);
            key.decrypt_in_place_detached(nonce, self.aad.as_slice(), &mut buffer, tag)
            .map_err(|e| CryptoError::EncodingError(format!("{e:?}")))?;
            Ok(buffer)
        }
    }

    fn algorithm_name(&self) -> &'static str {
        match self.algorithm {
            CipherAlgorithm::Aes128Ctr => "aes-128-ctr",
            CipherAlgorithm::Aes256Ctr => "aes-256-ctr",
            CipherAlgorithm::Aes128Cbc => "aes-128-cbc",
            CipherAlgorithm::Aes256Cbc => "aes-256-cbc",
            CipherAlgorithm::Aes128Gcm => "aes-128-gcm",
            CipherAlgorithm::Aes256Gcm => "aes-256-gcm",
            CipherAlgorithm::ChaCha20Poly1305 => "chacha20-poly1305",
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

/// Get supported hash algorithms.
pub fn get_hashes() -> Vec<&'static str> {
    vec!["md5", "sha1", "sha256", "sha384", "sha512"]
}

/// Get supported cipher algorithms.
pub fn get_ciphers() -> Vec<&'static str> {
    vec![
        "aes-128-ctr",
        "aes-256-ctr",
        "aes-128-cbc",
        "aes-256-cbc",
        "aes-128-gcm",
        "aes-256-gcm",
        "chacha20-poly1305",
    ]
}

/// Get supported elliptic curves (empty for now).
pub fn get_curves() -> Vec<&'static str> {
    Vec::new()
}

/// Constant-time equality check. Returns error on length mismatch.
pub fn timing_safe_equal(a: &[u8], b: &[u8]) -> Result<bool, CryptoError> {
    if a.len() != b.len() {
        return Err(CryptoError::InvalidKeyLength);
    }
    let mut diff: u8 = 0;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    Ok(diff == 0)
}

/// PBKDF2 key derivation.
pub fn pbkdf2(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    key_len: usize,
    digest: &str,
) -> Result<Vec<u8>, CryptoError> {
    let mut out = vec![0u8; key_len];
    match digest.to_lowercase().as_str() {
        "sha1" | "sha-1" => pbkdf2_hmac::<Sha1>(password, salt, iterations, &mut out),
        "sha256" | "sha-256" => pbkdf2_hmac::<Sha256>(password, salt, iterations, &mut out),
        "sha384" | "sha-384" => pbkdf2_hmac::<Sha384>(password, salt, iterations, &mut out),
        "sha512" | "sha-512" => pbkdf2_hmac::<Sha512>(password, salt, iterations, &mut out),
        _ => return Err(CryptoError::UnsupportedAlgorithm(digest.to_string())),
    };
    Ok(out)
}

/// scrypt key derivation.
pub fn scrypt(
    password: &[u8],
    salt: &[u8],
    key_len: usize,
    n: u64,
    r: u32,
    p: u32,
) -> Result<Vec<u8>, CryptoError> {
    if n == 0 || (n & (n - 1)) != 0 {
        return Err(CryptoError::InvalidParams(
            "N must be a power of two".to_string(),
        ));
    }
    let log_n = (64 - n.leading_zeros() - 1) as u8;
    let params = ScryptParams::new(log_n, r, p, key_len)
        .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
    let mut out = vec![0u8; key_len];
    scrypt_kdf(password, salt, &params, &mut out)
        .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFormat {
    Pem,
    Der,
}

impl KeyFormat {
    pub fn parse(value: &str) -> Result<Self, CryptoError> {
        match value.to_lowercase().as_str() {
            "pem" => Ok(Self::Pem),
            "der" => Ok(Self::Der),
            _ => Err(CryptoError::InvalidParams(format!(
                "Unsupported key format: {}",
                value
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Pkcs1,
    Pkcs8,
    Spki,
    Sec1,
}

impl KeyType {
    pub fn parse(value: &str) -> Result<Self, CryptoError> {
        match value.to_lowercase().as_str() {
            "pkcs1" => Ok(Self::Pkcs1),
            "pkcs8" => Ok(Self::Pkcs8),
            "spki" => Ok(Self::Spki),
            "sec1" => Ok(Self::Sec1),
            _ => Err(CryptoError::InvalidParams(format!(
                "Unsupported key type: {}",
                value
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsaEncoding {
    Der,
    IeeeP1363,
}

impl DsaEncoding {
    pub fn parse(value: &str) -> Result<Self, CryptoError> {
        match value.to_lowercase().as_str() {
            "der" => Ok(Self::Der),
            "ieee-p1363" => Ok(Self::IeeeP1363),
            _ => Err(CryptoError::InvalidParams(format!(
                "Unsupported dsaEncoding: {}",
                value
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsaPadding {
    Pkcs1,
    Pss,
}

#[derive(Debug, Clone)]
pub struct SignOptions {
    pub dsa_encoding: Option<DsaEncoding>,
    pub padding: Option<RsaPadding>,
    pub salt_length: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct KeyInput {
    pub data: Vec<u8>,
    pub format: KeyFormat,
    pub key_type: Option<KeyType>,
}

#[derive(Debug, Clone)]
pub struct KeyPairOutput {
    pub public_key: KeyOutput,
    pub private_key: KeyOutput,
}

#[derive(Debug, Clone)]
pub enum KeyOutput {
    Pem(String),
    Der(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct KeyPairOptions {
    pub key_type: String,
    pub modulus_length: Option<usize>,
    pub public_exponent: Option<u64>,
    pub named_curve: Option<String>,
    pub public_key_format: KeyFormat,
    pub public_key_type: KeyType,
    pub private_key_format: KeyFormat,
    pub private_key_type: KeyType,
}

#[derive(Debug, Clone)]
pub struct SubtleAesGcmOptions {
    pub iv: Vec<u8>,
    pub additional_data: Option<Vec<u8>>,
    pub tag_length: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JwkKey {
    pub kty: String,
    pub crv: Option<String>,
    pub x: Option<String>,
    pub y: Option<String>,
    pub d: Option<String>,
    pub n: Option<String>,
    pub e: Option<String>,
    pub p: Option<String>,
    pub q: Option<String>,
    pub dp: Option<String>,
    pub dq: Option<String>,
    pub qi: Option<String>,
    pub k: Option<String>,
    pub alg: Option<String>,
    pub key_ops: Option<Vec<String>>,
    pub ext: Option<bool>,
}

fn b64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn b64url_decode(value: &str) -> Result<Vec<u8>, CryptoError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|e| CryptoError::EncodingError(e.to_string()))
}

fn pad_left(bytes: &[u8], len: usize) -> Vec<u8> {
    if bytes.len() >= len {
        return bytes.to_vec();
    }
    let mut out = vec![0u8; len - bytes.len()];
    out.extend_from_slice(bytes);
    out
}

fn parse_pem(data: &[u8]) -> Result<Pem, CryptoError> {
    pem::parse(data).map_err(|e| CryptoError::EncodingError(e.to_string()))
}

fn pem_key_type(label: &str) -> Option<KeyType> {
    match label {
        "RSA PRIVATE KEY" => Some(KeyType::Pkcs1),
        "RSA PUBLIC KEY" => Some(KeyType::Pkcs1),
        "PRIVATE KEY" => Some(KeyType::Pkcs8),
        "PUBLIC KEY" => Some(KeyType::Spki),
        "EC PRIVATE KEY" => Some(KeyType::Sec1),
        _ => None,
    }
}

fn parse_key_material(key: &KeyInput) -> Result<(Vec<u8>, Option<KeyType>), CryptoError> {
    match key.format {
        KeyFormat::Pem => {
            let pem = parse_pem(&key.data)?;
            let key_type = key.key_type.or_else(|| pem_key_type(pem.tag()));
            Ok((pem.contents().to_vec(), key_type))
        }
        KeyFormat::Der => Ok((key.data.clone(), key.key_type)),
    }
}

fn resolve_hash_algorithm(name: &str) -> Result<HashAlgorithm, CryptoError> {
    HashAlgorithm::parse(name)
}

fn resolve_signature_hash(name: &str) -> Result<HashAlgorithm, CryptoError> {
    match name.to_lowercase().as_str() {
        "rsa-sha1" | "sha1" | "sha-1" => Ok(HashAlgorithm::Sha1),
        "rsa-sha256" | "sha256" | "sha-256" => Ok(HashAlgorithm::Sha256),
        "rsa-sha384" | "sha384" | "sha-384" => Ok(HashAlgorithm::Sha384),
        "rsa-sha512" | "sha512" | "sha-512" => Ok(HashAlgorithm::Sha512),
        "ecdsa-with-sha256" => Ok(HashAlgorithm::Sha256),
        _ => Err(CryptoError::UnsupportedAlgorithm(name.to_string())),
    }
}

fn resolve_rsa_padding(algorithm: &str, options: &SignOptions) -> RsaPadding {
    if let Some(padding) = options.padding {
        return padding;
    }
    if algorithm.to_lowercase().contains("pss") {
        return RsaPadding::Pss;
    }
    RsaPadding::Pkcs1
}

fn parse_rsa_private(key: &KeyInput) -> Result<RsaPrivateKey, CryptoError> {
    let (der, key_type) = parse_key_material(key)?;
    match key_type {
        Some(KeyType::Pkcs1) => RsaPrivateKey::from_pkcs1_der(&der)
            .map_err(|e| CryptoError::EncodingError(e.to_string())),
        Some(KeyType::Pkcs8) | None => RsaPrivateKey::from_pkcs8_der(&der)
            .map_err(|e| CryptoError::EncodingError(e.to_string())),
        _ => Err(CryptoError::InvalidParams(
            "Invalid RSA private key type".to_string(),
        )),
    }
}

fn parse_rsa_public(key: &KeyInput) -> Result<RsaPublicKey, CryptoError> {
    let (der, key_type) = parse_key_material(key)?;
    match key_type {
        Some(KeyType::Pkcs1) => RsaPublicKey::from_pkcs1_der(&der)
            .map_err(|e| CryptoError::EncodingError(e.to_string())),
        Some(KeyType::Spki) | Some(KeyType::Pkcs8) | None => {
            RsaPublicKey::from_public_key_der(&der)
                .map_err(|e| CryptoError::EncodingError(e.to_string()))
        }
        _ => Err(CryptoError::InvalidParams(
            "Invalid RSA public key type".to_string(),
        )),
    }
}

fn parse_p256_private(key: &KeyInput) -> Result<P256SigningKey, CryptoError> {
    let (der, key_type) = parse_key_material(key)?;
    match key_type {
        Some(KeyType::Sec1) => {
            let secret = P256SecretKey::from_sec1_der(&der)
                .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
            Ok(P256SigningKey::from(secret))
        }
        Some(KeyType::Pkcs8) | None => P256SigningKey::from_pkcs8_der(&der)
            .map_err(|e| CryptoError::EncodingError(e.to_string())),
        _ => Err(CryptoError::InvalidParams(
            "Invalid EC private key type".to_string(),
        )),
    }
}

fn parse_p256_public(key: &KeyInput) -> Result<P256VerifyingKey, CryptoError> {
    let (der, key_type) = parse_key_material(key)?;
    match key_type {
        Some(KeyType::Spki) | Some(KeyType::Pkcs8) | None => {
            P256VerifyingKey::from_public_key_der(&der)
                .map_err(|e| CryptoError::EncodingError(e.to_string()))
        }
        _ => Err(CryptoError::InvalidParams(
            "Invalid EC public key type".to_string(),
        )),
    }
}

fn parse_p256_secret(key: &KeyInput) -> Result<P256SecretKey, CryptoError> {
    let (der, key_type) = parse_key_material(key)?;
    match key_type {
        Some(KeyType::Sec1) => P256SecretKey::from_sec1_der(&der)
            .map_err(|e| CryptoError::EncodingError(e.to_string())),
        Some(KeyType::Pkcs8) | None => P256SecretKey::from_pkcs8_der(&der)
            .map_err(|e| CryptoError::EncodingError(e.to_string())),
        _ => Err(CryptoError::InvalidParams(
            "Invalid EC private key type".to_string(),
        )),
    }
}

fn parse_p256_public_key(key: &KeyInput) -> Result<P256PublicKey, CryptoError> {
    let (der, key_type) = parse_key_material(key)?;
    match key_type {
        Some(KeyType::Spki) | Some(KeyType::Pkcs8) | None => {
            P256PublicKey::from_public_key_der(&der)
                .map_err(|e| CryptoError::EncodingError(e.to_string()))
        }
        _ => Err(CryptoError::InvalidParams(
            "Invalid EC public key type".to_string(),
        )),
    }
}

fn parse_ed25519_private(key: &KeyInput) -> Result<Ed25519SigningKey, CryptoError> {
    let (der, key_type) = parse_key_material(key)?;
    match key_type {
        Some(KeyType::Pkcs8) | None => Ed25519SigningKey::from_pkcs8_der(&der)
            .map_err(|e| CryptoError::EncodingError(e.to_string())),
        _ => Err(CryptoError::InvalidParams(
            "Invalid Ed25519 private key type".to_string(),
        )),
    }
}

fn parse_ed25519_public(key: &KeyInput) -> Result<Ed25519VerifyingKey, CryptoError> {
    let (der, key_type) = parse_key_material(key)?;
    match key_type {
        Some(KeyType::Spki) | Some(KeyType::Pkcs8) | None => {
            Ed25519VerifyingKey::from_public_key_der(&der)
                .map_err(|e| CryptoError::EncodingError(e.to_string()))
        }
        _ => Err(CryptoError::InvalidParams(
            "Invalid Ed25519 public key type".to_string(),
        )),
    }
}

fn sign_rsa(
    hash_alg: HashAlgorithm,
    key: &KeyInput,
    data: &[u8],
    options: &SignOptions,
    padding: RsaPadding,
) -> Result<Vec<u8>, CryptoError> {
    let private_key = parse_rsa_private(key)?;
    let digest = hash(hash_alg.to_string().as_str(), data)?;
    let mut rng = OsRng;
    let signature = match (padding, hash_alg) {
        (RsaPadding::Pkcs1, HashAlgorithm::Sha1) => {
            private_key.sign(Pkcs1v15Sign::new::<Sha1>(), &digest)
        }
        (RsaPadding::Pkcs1, HashAlgorithm::Sha256) => {
            private_key.sign(Pkcs1v15Sign::new::<Sha256>(), &digest)
        }
        (RsaPadding::Pkcs1, HashAlgorithm::Sha384) => {
            private_key.sign(Pkcs1v15Sign::new::<Sha384>(), &digest)
        }
        (RsaPadding::Pkcs1, HashAlgorithm::Sha512) => {
            private_key.sign(Pkcs1v15Sign::new::<Sha512>(), &digest)
        }
        (RsaPadding::Pss, HashAlgorithm::Sha1) => {
            let scheme = options
                .salt_length
                .map(|salt| Pss::new_with_salt::<Sha1>(salt))
                .unwrap_or_else(Pss::new::<Sha1>);
            private_key.sign_with_rng(&mut rng, scheme, &digest)
        }
        (RsaPadding::Pss, HashAlgorithm::Sha256) => {
            let scheme = options
                .salt_length
                .map(|salt| Pss::new_with_salt::<Sha256>(salt))
                .unwrap_or_else(Pss::new::<Sha256>);
            private_key.sign_with_rng(&mut rng, scheme, &digest)
        }
        (RsaPadding::Pss, HashAlgorithm::Sha384) => {
            let scheme = options
                .salt_length
                .map(|salt| Pss::new_with_salt::<Sha384>(salt))
                .unwrap_or_else(Pss::new::<Sha384>);
            private_key.sign_with_rng(&mut rng, scheme, &digest)
        }
        (RsaPadding::Pss, HashAlgorithm::Sha512) => {
            let scheme = options
                .salt_length
                .map(|salt| Pss::new_with_salt::<Sha512>(salt))
                .unwrap_or_else(Pss::new::<Sha512>);
            private_key.sign_with_rng(&mut rng, scheme, &digest)
        }
        _ => {
            return Err(CryptoError::UnsupportedAlgorithm(format!(
                "Unsupported RSA hash {:?}",
                hash_alg
            )))
        }
    }
    .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
    Ok(signature)
}

fn verify_rsa(
    hash_alg: HashAlgorithm,
    key: &KeyInput,
    data: &[u8],
    signature: &[u8],
    options: &SignOptions,
    padding: RsaPadding,
) -> Result<bool, CryptoError> {
    let public_key = parse_rsa_public(key)?;
    let digest = hash(hash_alg.to_string().as_str(), data)?;
    let result = match (padding, hash_alg) {
        (RsaPadding::Pkcs1, HashAlgorithm::Sha1) => {
            public_key.verify(Pkcs1v15Sign::new::<Sha1>(), &digest, signature)
        }
        (RsaPadding::Pkcs1, HashAlgorithm::Sha256) => {
            public_key.verify(Pkcs1v15Sign::new::<Sha256>(), &digest, signature)
        }
        (RsaPadding::Pkcs1, HashAlgorithm::Sha384) => {
            public_key.verify(Pkcs1v15Sign::new::<Sha384>(), &digest, signature)
        }
        (RsaPadding::Pkcs1, HashAlgorithm::Sha512) => {
            public_key.verify(Pkcs1v15Sign::new::<Sha512>(), &digest, signature)
        }
        (RsaPadding::Pss, HashAlgorithm::Sha1) => {
            let scheme = options
                .salt_length
                .map(|salt| Pss::new_with_salt::<Sha1>(salt))
                .unwrap_or_else(Pss::new::<Sha1>);
            public_key.verify(scheme, &digest, signature)
        }
        (RsaPadding::Pss, HashAlgorithm::Sha256) => {
            let scheme = options
                .salt_length
                .map(|salt| Pss::new_with_salt::<Sha256>(salt))
                .unwrap_or_else(Pss::new::<Sha256>);
            public_key.verify(scheme, &digest, signature)
        }
        (RsaPadding::Pss, HashAlgorithm::Sha384) => {
            let scheme = options
                .salt_length
                .map(|salt| Pss::new_with_salt::<Sha384>(salt))
                .unwrap_or_else(Pss::new::<Sha384>);
            public_key.verify(scheme, &digest, signature)
        }
        (RsaPadding::Pss, HashAlgorithm::Sha512) => {
            let scheme = options
                .salt_length
                .map(|salt| Pss::new_with_salt::<Sha512>(salt))
                .unwrap_or_else(Pss::new::<Sha512>);
            public_key.verify(scheme, &digest, signature)
        }
        _ => {
            return Err(CryptoError::UnsupportedAlgorithm(format!(
                "Unsupported RSA hash {:?}",
                hash_alg
            )))
        }
    };
    Ok(result.is_ok())
}

fn sign_ecdsa_p256(
    hash: HashAlgorithm,
    key: &KeyInput,
    data: &[u8],
    options: &SignOptions,
) -> Result<Vec<u8>, CryptoError> {
    if hash != HashAlgorithm::Sha256 {
        return Err(CryptoError::UnsupportedAlgorithm(
            "P-256 only supports SHA-256".to_string(),
        ));
    }
    let signing_key = parse_p256_private(key)?;
    let signature: P256Signature = signing_key.sign(data);
    let encoding = options.dsa_encoding.unwrap_or(DsaEncoding::Der);
    match encoding {
        DsaEncoding::Der => Ok(signature.to_der().as_bytes().to_vec()),
        DsaEncoding::IeeeP1363 => Ok(signature.to_bytes().to_vec()),
    }
}

fn verify_ecdsa_p256(
    hash: HashAlgorithm,
    key: &KeyInput,
    data: &[u8],
    signature: &[u8],
    options: &SignOptions,
) -> Result<bool, CryptoError> {
    if hash != HashAlgorithm::Sha256 {
        return Err(CryptoError::UnsupportedAlgorithm(
            "P-256 only supports SHA-256".to_string(),
        ));
    }
    let verifying_key = parse_p256_public(key)?;
    let encoding = options.dsa_encoding.unwrap_or(DsaEncoding::Der);
    let signature = match encoding {
        DsaEncoding::Der => P256Signature::from_der(signature)
            .map_err(|e| CryptoError::EncodingError(e.to_string()))?,
        DsaEncoding::IeeeP1363 => P256Signature::from_slice(signature)
            .map_err(|e| CryptoError::EncodingError(e.to_string()))?,
    };
    Ok(verifying_key.verify(data, &signature).is_ok())
}

fn sign_ed25519(key: &KeyInput, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let signing_key = parse_ed25519_private(key)?;
    let signature = signing_key.sign(data);
    Ok(signature.to_bytes().to_vec())
}

fn verify_ed25519(
    key: &KeyInput,
    data: &[u8],
    signature: &[u8],
) -> Result<bool, CryptoError> {
    let verifying_key = parse_ed25519_public(key)?;
    let signature = ed25519_dalek::Signature::try_from(signature)
        .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
    Ok(verifying_key.verify_strict(data, &signature).is_ok())
}

pub fn sign(
    algorithm: &str,
    key: &KeyInput,
    data: &[u8],
    options: &SignOptions,
) -> Result<Vec<u8>, CryptoError> {
    let name = algorithm.to_lowercase();
    if name.contains("ed25519") {
        return sign_ed25519(key, data);
    }
    let hash = resolve_signature_hash(&name)?;
    if name.contains("ecdsa") || name.contains("ec") {
        return sign_ecdsa_p256(hash, key, data, options);
    }
    let padding = resolve_rsa_padding(&name, options);
    sign_rsa(hash, key, data, options, padding)
}

pub fn verify(
    algorithm: &str,
    key: &KeyInput,
    data: &[u8],
    signature: &[u8],
    options: &SignOptions,
) -> Result<bool, CryptoError> {
    let name = algorithm.to_lowercase();
    if name.contains("ed25519") {
        return verify_ed25519(key, data, signature);
    }
    let hash = resolve_signature_hash(&name)?;
    if name.contains("ecdsa") || name.contains("ec") {
        return verify_ecdsa_p256(hash, key, data, signature, options);
    }
    let padding = resolve_rsa_padding(&name, options);
    verify_rsa(hash, key, data, signature, options, padding)
}

fn jwk_biguint(value: &str) -> Result<rsa::BigUint, CryptoError> {
    let bytes = b64url_decode(value)?;
    Ok(rsa::BigUint::from_bytes_be(&bytes))
}

fn jwk_biguint_to_b64(value: &rsa::BigUint) -> String {
    b64url_encode(&value.to_bytes_be())
}

fn jwk_to_rsa_key(jwk: &JwkKey) -> Result<(Option<RsaPrivateKey>, RsaPublicKey), CryptoError> {
    let n = jwk
        .n
        .as_deref()
        .ok_or_else(|| CryptoError::InvalidParams("Missing RSA modulus".to_string()))
        .and_then(jwk_biguint)?;
    let e = jwk
        .e
        .as_deref()
        .ok_or_else(|| CryptoError::InvalidParams("Missing RSA exponent".to_string()))
        .and_then(jwk_biguint)?;
    let public_key = RsaPublicKey::new(n.clone(), e.clone())
        .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;

    if let Some(d) = jwk.d.as_deref() {
        let d = jwk_biguint(d)?;
        let p = jwk
            .p
            .as_deref()
            .ok_or_else(|| CryptoError::InvalidParams("Missing RSA prime p".to_string()))
            .and_then(jwk_biguint)?;
        let q = jwk
            .q
            .as_deref()
            .ok_or_else(|| CryptoError::InvalidParams("Missing RSA prime q".to_string()))
            .and_then(jwk_biguint)?;
        let private_key =
            RsaPrivateKey::from_components(n, e, d, vec![p, q])
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
        Ok((Some(private_key), public_key))
    } else {
        Ok((None, public_key))
    }
}

fn jwk_to_p256_key(jwk: &JwkKey) -> Result<(Option<P256SecretKey>, P256PublicKey), CryptoError> {
    let x = jwk
        .x
        .as_deref()
        .ok_or_else(|| CryptoError::InvalidParams("Missing EC x".to_string()))
        .and_then(b64url_decode)?;
    let y = jwk
        .y
        .as_deref()
        .ok_or_else(|| CryptoError::InvalidParams("Missing EC y".to_string()))
        .and_then(b64url_decode)?;
    let x = pad_left(&x, 32);
    let y = pad_left(&y, 32);
    let x = P256FieldBytes::from_slice(&x);
    let y = P256FieldBytes::from_slice(&y);
    let point = P256EncodedPoint::from_affine_coordinates(x, y, false);
    let public_key = P256PublicKey::from_sec1_bytes(point.as_bytes())
        .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;

    if let Some(d) = jwk.d.as_deref() {
        let d = b64url_decode(d)?;
        let d = pad_left(&d, 32);
        let secret = P256SecretKey::from_slice(&d)
            .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
        Ok((Some(secret), public_key))
    } else {
        Ok((None, public_key))
    }
}

pub fn jwk_to_der(jwk: &JwkKey) -> Result<(Vec<u8>, KeyType), CryptoError> {
    match jwk.kty.to_uppercase().as_str() {
        "RSA" => {
            let (private_key, public_key) = jwk_to_rsa_key(jwk)?;
            if let Some(private_key) = private_key {
                let der = private_key
                    .to_pkcs8_der()
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                    .as_bytes()
                    .to_vec();
                Ok((der, KeyType::Pkcs8))
            } else {
                let der = public_key
                    .to_public_key_der()
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                    .as_bytes()
                    .to_vec();
                Ok((der, KeyType::Spki))
            }
        }
        "EC" => {
            let (private_key, public_key) = jwk_to_p256_key(jwk)?;
            if let Some(private_key) = private_key {
                let der = private_key
                    .to_pkcs8_der()
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                    .as_bytes()
                    .to_vec();
                Ok((der, KeyType::Pkcs8))
            } else {
                let der = public_key
                    .to_public_key_der()
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                    .as_bytes()
                    .to_vec();
                Ok((der, KeyType::Spki))
            }
        }
        "OCT" => {
            let k = jwk
                .k
                .as_deref()
                .ok_or_else(|| CryptoError::InvalidParams("Missing symmetric key".to_string()))
                .and_then(b64url_decode)?;
            Ok((k, KeyType::Pkcs8))
        }
        other => Err(CryptoError::InvalidParams(format!(
            "Unsupported JWK kty: {}",
            other
        ))),
    }
}

fn rsa_public_to_jwk(public_key: &RsaPublicKey) -> JwkKey {
    JwkKey {
        kty: "RSA".to_string(),
        crv: None,
        x: None,
        y: None,
        d: None,
        n: Some(jwk_biguint_to_b64(public_key.n())),
        e: Some(jwk_biguint_to_b64(public_key.e())),
        p: None,
        q: None,
        dp: None,
        dq: None,
        qi: None,
        k: None,
        alg: None,
        key_ops: None,
        ext: None,
    }
}

fn rsa_private_to_jwk(private_key: &RsaPrivateKey) -> JwkKey {
    let mut jwk = rsa_public_to_jwk(&RsaPublicKey::from(private_key));
    jwk.d = Some(jwk_biguint_to_b64(private_key.d()));
    if let Some(primes) = private_key.primes().get(0..2) {
        jwk.p = Some(jwk_biguint_to_b64(&primes[0]));
        jwk.q = Some(jwk_biguint_to_b64(&primes[1]));
    }
    if let Some(dp) = private_key.dp() {
        jwk.dp = Some(jwk_biguint_to_b64(dp));
    }
    if let Some(dq) = private_key.dq() {
        jwk.dq = Some(jwk_biguint_to_b64(dq));
    }
    if let Some(qinv) = private_key.qinv() {
        let value = qinv.to_biguint().unwrap_or_default();
        jwk.qi = Some(jwk_biguint_to_b64(&value));
    }
    jwk
}

fn p256_public_to_jwk(public_key: &P256PublicKey) -> JwkKey {
    let point = public_key.to_encoded_point(false);
    let x = point.x().map(|x| b64url_encode(x)).unwrap_or_default();
    let y = point.y().map(|y| b64url_encode(y)).unwrap_or_default();
    JwkKey {
        kty: "EC".to_string(),
        crv: Some("P-256".to_string()),
        x: Some(x),
        y: Some(y),
        d: None,
        n: None,
        e: None,
        p: None,
        q: None,
        dp: None,
        dq: None,
        qi: None,
        k: None,
        alg: None,
        key_ops: None,
        ext: None,
    }
}

fn p256_private_to_jwk(secret: &P256SecretKey) -> JwkKey {
    let public_key = secret.public_key();
    let mut jwk = p256_public_to_jwk(&public_key);
    jwk.d = Some(b64url_encode(secret.to_bytes().as_slice()));
    jwk
}

pub fn der_to_jwk(der: &[u8], key_type: KeyType, algorithm: &str) -> Result<JwkKey, CryptoError> {
    match algorithm.to_uppercase().as_str() {
        "RSA" => {
            if matches!(key_type, KeyType::Spki | KeyType::Pkcs1) {
                let public_key = if key_type == KeyType::Pkcs1 {
                    RsaPublicKey::from_pkcs1_der(der)
                        .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                } else {
                    RsaPublicKey::from_public_key_der(der)
                        .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                };
                Ok(rsa_public_to_jwk(&public_key))
            } else {
                let private_key = if key_type == KeyType::Pkcs1 {
                    RsaPrivateKey::from_pkcs1_der(der)
                        .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                } else {
                    RsaPrivateKey::from_pkcs8_der(der)
                        .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                };
                Ok(rsa_private_to_jwk(&private_key))
            }
        }
        "EC" => {
            if matches!(key_type, KeyType::Spki) {
                let public_key = P256PublicKey::from_public_key_der(der)
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
                Ok(p256_public_to_jwk(&public_key))
            } else {
                let secret = P256SecretKey::from_pkcs8_der(der)
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?;
                Ok(p256_private_to_jwk(&secret))
            }
        }
        other => Err(CryptoError::UnsupportedAlgorithm(other.to_string())),
    }
}

fn encode_key_output(output: Vec<u8>, format: KeyFormat, label: &str) -> Result<KeyOutput, CryptoError> {
    match format {
        KeyFormat::Der => Ok(KeyOutput::Der(output)),
        KeyFormat::Pem => {
            let pem = Pem::new(label, output);
            Ok(KeyOutput::Pem(pem::encode(&pem)))
        }
    }
}

pub fn generate_key_pair(options: &KeyPairOptions) -> Result<KeyPairOutput, CryptoError> {
    match options.key_type.to_lowercase().as_str() {
        "rsa" => {
            let bits = options.modulus_length.unwrap_or(2048);
            let exponent = options.public_exponent.unwrap_or(65537);
            let exponent = rsa::BigUint::from(exponent);
            let mut rng = OsRng;
            let private_key = RsaPrivateKey::new_with_exp(&mut rng, bits, &exponent)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            let public_key = RsaPublicKey::from(&private_key);

            let private_der = match options.private_key_type {
                KeyType::Pkcs1 => private_key
                    .to_pkcs1_der()
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                    .as_bytes()
                    .to_vec(),
                KeyType::Pkcs8 => private_key
                    .to_pkcs8_der()
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                    .as_bytes()
                    .to_vec(),
                _ => {
                    return Err(CryptoError::InvalidParams(
                        "RSA private key must be pkcs1 or pkcs8".to_string(),
                    ))
                }
            };

            let public_der = match options.public_key_type {
                KeyType::Pkcs1 => public_key
                    .to_pkcs1_der()
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                    .as_bytes()
                    .to_vec(),
                KeyType::Spki => public_key
                    .to_public_key_der()
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                    .as_bytes()
                    .to_vec(),
                _ => {
                    return Err(CryptoError::InvalidParams(
                        "RSA public key must be spki or pkcs1".to_string(),
                    ))
                }
            };

            Ok(KeyPairOutput {
                public_key: encode_key_output(public_der, options.public_key_format, "PUBLIC KEY")?,
                private_key: encode_key_output(
                    private_der,
                    options.private_key_format,
                    "PRIVATE KEY",
                )?,
            })
        }
        "ec" | "ecdsa" => {
            let curve = options
                .named_curve
                .as_deref()
                .unwrap_or("prime256v1");
            if !matches!(curve.to_lowercase().as_str(), "prime256v1" | "secp256r1" | "p-256") {
                return Err(CryptoError::UnsupportedAlgorithm(format!(
                    "Unsupported curve: {}",
                    curve
                )));
            }
            let mut rng = OsRng;
            let secret = P256SecretKey::random(&mut rng);
            let public_key = secret.public_key();

            let private_der = match options.private_key_type {
                KeyType::Sec1 => secret
                    .to_sec1_der()
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                    .as_slice()
                    .to_vec(),
                KeyType::Pkcs8 => secret
                    .to_pkcs8_der()
                    .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                    .as_bytes()
                    .to_vec(),
                _ => {
                    return Err(CryptoError::InvalidParams(
                        "EC private key must be sec1 or pkcs8".to_string(),
                    ))
                }
            };

            let public_der = public_key
                .to_public_key_der()
                .map_err(|e| CryptoError::EncodingError(e.to_string()))?
                .as_bytes()
                .to_vec();

            Ok(KeyPairOutput {
                public_key: encode_key_output(public_der, options.public_key_format, "PUBLIC KEY")?,
                private_key: encode_key_output(
                    private_der,
                    options.private_key_format,
                    "PRIVATE KEY",
                )?,
            })
        }
        "ed25519" => {
            let mut rng = OsRng;
            let mut secret = [0u8; 32];
            rng.fill_bytes(&mut secret);
            let signing_key = Ed25519SigningKey::from_bytes(&secret);
            let verifying_key = signing_key.verifying_key();

            let private_der = signing_key
                .to_pkcs8_der()
                .map_err(|e: pkcs8::Error| CryptoError::EncodingError(e.to_string()))?
                .as_bytes()
                .to_vec();

            let public_der = verifying_key
                .to_public_key_der()
                .map_err(|e: spki::Error| CryptoError::EncodingError(e.to_string()))?
                .as_bytes()
                .to_vec();

            Ok(KeyPairOutput {
                public_key: encode_key_output(public_der, options.public_key_format, "PUBLIC KEY")?,
                private_key: encode_key_output(
                    private_der,
                    options.private_key_format,
                    "PRIVATE KEY",
                )?,
            })
        }
        other => Err(CryptoError::UnsupportedAlgorithm(other.to_string())),
    }
}

pub fn subtle_digest(algorithm: &str, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let alg = resolve_hash_algorithm(algorithm)?;
    hash(alg.to_string().as_str(), data)
}

pub fn subtle_encrypt_aes_gcm(
    key: &[u8],
    data: &[u8],
    options: &SubtleAesGcmOptions,
) -> Result<Vec<u8>, CryptoError> {
    let tag_len = options.tag_length.unwrap_or(128);
    if tag_len != 128 {
        return Err(CryptoError::InvalidParams(
            "Only 128-bit tags are supported".to_string(),
        ));
    }
    let mut buffer = data.to_vec();
    let aad = options
        .additional_data
        .as_ref()
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let tag = match key.len() {
        16 => {
            let cipher = Aes128Gcm::new_from_slice(key)
                .map_err(|_| CryptoError::InvalidKeyLength)?;
            let nonce = aes_gcm::Nonce::from_slice(&options.iv);
            cipher
                .encrypt_in_place_detached(nonce, aad, &mut buffer)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?
        }
        32 => {
            let cipher = Aes256Gcm::new_from_slice(key)
                .map_err(|_| CryptoError::InvalidKeyLength)?;
            let nonce = aes_gcm::Nonce::from_slice(&options.iv);
            cipher
                .encrypt_in_place_detached(nonce, aad, &mut buffer)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?
        }
        _ => return Err(CryptoError::InvalidKeyLength),
    };
    buffer.extend_from_slice(tag.as_slice());
    Ok(buffer)
}

pub fn subtle_decrypt_aes_gcm(
    key: &[u8],
    data: &[u8],
    options: &SubtleAesGcmOptions,
) -> Result<Vec<u8>, CryptoError> {
    let tag_len = options.tag_length.unwrap_or(128);
    if tag_len != 128 {
        return Err(CryptoError::InvalidParams(
            "Only 128-bit tags are supported".to_string(),
        ));
    }
    if data.len() < 16 {
        return Err(CryptoError::InvalidParams(
            "Ciphertext too short".to_string(),
        ));
    }
    let split_at = data.len() - 16;
    let mut buffer = data[..split_at].to_vec();
    let tag = &data[split_at..];
    let aad = options
        .additional_data
        .as_ref()
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let result = match key.len() {
        16 => {
            let cipher = Aes128Gcm::new_from_slice(key)
                .map_err(|_| CryptoError::InvalidKeyLength)?;
            let nonce = aes_gcm::Nonce::from_slice(&options.iv);
            cipher.decrypt_in_place_detached(
                nonce,
                aad,
                &mut buffer,
                tag.into(),
            )
        }
        32 => {
            let cipher = Aes256Gcm::new_from_slice(key)
                .map_err(|_| CryptoError::InvalidKeyLength)?;
            let nonce = aes_gcm::Nonce::from_slice(&options.iv);
            cipher.decrypt_in_place_detached(
                nonce,
                aad,
                &mut buffer,
                tag.into(),
            )
        }
        _ => return Err(CryptoError::InvalidKeyLength),
    };
    result.map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
    Ok(buffer)
}

fn increment_counter(counter: &mut [u8; 16], length_bits: u32) {
    let mut carry = true;
    let mut bit_index = 0;
    while carry && bit_index < length_bits {
        let byte_index = 15 - (bit_index / 8) as usize;
        let bit_offset = (bit_index % 8) as u8;
        let mask = 1u8 << bit_offset;
        if counter[byte_index] & mask == 0 {
            counter[byte_index] |= mask;
            carry = false;
        } else {
            counter[byte_index] &= !mask;
        }
        bit_index += 1;
    }
}

fn aes_ctr_crypt(
    key: &[u8],
    counter: &[u8],
    length_bits: u32,
    data: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if counter.len() != 16 {
        return Err(CryptoError::InvalidParams(
            "AES-CTR counter must be 16 bytes".to_string(),
        ));
    }
    if length_bits == 0 || length_bits > 128 {
        return Err(CryptoError::InvalidParams(
            "AES-CTR length must be between 1 and 128".to_string(),
        ));
    }
    let mut counter_block = [0u8; 16];
    counter_block.copy_from_slice(counter);
    let mut output = vec![0u8; data.len()];

    match key.len() {
        16 => {
            let cipher = Aes128::new_from_slice(key).map_err(|_| CryptoError::InvalidKeyLength)?;
            for (chunk, out_chunk) in data.chunks(16).zip(output.chunks_mut(16)) {
                let mut block = cipher::generic_array::GenericArray::clone_from_slice(&counter_block);
                cipher.encrypt_block(&mut block);
                for (i, byte) in chunk.iter().enumerate() {
                    out_chunk[i] = byte ^ block[i];
                }
                increment_counter(&mut counter_block, length_bits);
            }
        }
        32 => {
            let cipher = Aes256::new_from_slice(key).map_err(|_| CryptoError::InvalidKeyLength)?;
            for (chunk, out_chunk) in data.chunks(16).zip(output.chunks_mut(16)) {
                let mut block = cipher::generic_array::GenericArray::clone_from_slice(&counter_block);
                cipher.encrypt_block(&mut block);
                for (i, byte) in chunk.iter().enumerate() {
                    out_chunk[i] = byte ^ block[i];
                }
                increment_counter(&mut counter_block, length_bits);
            }
        }
        _ => return Err(CryptoError::InvalidKeyLength),
    }

    Ok(output)
}

pub fn subtle_encrypt_aes_cbc(
    key: &[u8],
    data: &[u8],
    iv: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if data.len() % 16 != 0 {
        return Err(CryptoError::InvalidParams(
            "AES-CBC data length must be multiple of 16".to_string(),
        ));
    }
    match key.len() {
        16 => {
            let mut buffer = data.to_vec();
            let mut cipher = cbc::Encryptor::<Aes128>::new(key.into(), iv.into());
            let size = buffer.len();
            cipher
                .encrypt_padded_mut::<NoPadding>(&mut buffer, size)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            Ok(buffer)
        }
        32 => {
            let mut buffer = data.to_vec();
            let mut cipher = cbc::Encryptor::<Aes256>::new(key.into(), iv.into());
            let size = buffer.len();
            cipher
                .encrypt_padded_mut::<NoPadding>(&mut buffer, size)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            Ok(buffer)
        }
        _ => Err(CryptoError::InvalidKeyLength),
    }
}

pub fn subtle_decrypt_aes_cbc(
    key: &[u8],
    data: &[u8],
    iv: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if data.len() % 16 != 0 {
        return Err(CryptoError::InvalidParams(
            "AES-CBC data length must be multiple of 16".to_string(),
        ));
    }
    match key.len() {
        16 => {
            let mut buffer = data.to_vec();
            let mut cipher = cbc::Decryptor::<Aes128>::new(key.into(), iv.into());
            let size = buffer.len();
            cipher
                .decrypt_padded_mut::<NoPadding>(&mut buffer)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            buffer.truncate(size);
            Ok(buffer)
        }
        32 => {
            let mut buffer = data.to_vec();
            let mut cipher = cbc::Decryptor::<Aes256>::new(key.into(), iv.into());
            let size = buffer.len();
            cipher
                .decrypt_padded_mut::<NoPadding>(&mut buffer)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            buffer.truncate(size);
            Ok(buffer)
        }
        _ => Err(CryptoError::InvalidKeyLength),
    }
}

pub fn subtle_encrypt_aes_ctr(
    key: &[u8],
    data: &[u8],
    counter: &[u8],
    length_bits: u32,
) -> Result<Vec<u8>, CryptoError> {
    aes_ctr_crypt(key, counter, length_bits, data)
}

pub fn subtle_decrypt_aes_ctr(
    key: &[u8],
    data: &[u8],
    counter: &[u8],
    length_bits: u32,
) -> Result<Vec<u8>, CryptoError> {
    aes_ctr_crypt(key, counter, length_bits, data)
}

pub fn subtle_wrap_aes_kw(key: &[u8], data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let output = match key.len() {
        16 => {
            let kek = Kek::<Aes128>::try_from(key)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            let mut out = vec![0u8; data.len() + 8];
            kek.wrap(data, &mut out)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            Ok(out)
        }
        24 => {
            let kek = Kek::<Aes192>::try_from(key)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            let mut out = vec![0u8; data.len() + 8];
            kek.wrap(data, &mut out)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            Ok(out)
        }
        32 => {
            let kek = Kek::<Aes256>::try_from(key)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            let mut out = vec![0u8; data.len() + 8];
            kek.wrap(data, &mut out)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            Ok(out)
        }
        _ => return Err(CryptoError::InvalidKeyLength),
    }?;
    Ok(output)
}

pub fn subtle_unwrap_aes_kw(key: &[u8], data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let output = match key.len() {
        16 => {
            let kek = Kek::<Aes128>::try_from(key)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            if data.len() < 8 {
                return Err(CryptoError::InvalidParams("AES-KW data too short".to_string()));
            }
            let mut out = vec![0u8; data.len() - 8];
            kek.unwrap(data, &mut out)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            Ok(out)
        }
        24 => {
            let kek = Kek::<Aes192>::try_from(key)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            if data.len() < 8 {
                return Err(CryptoError::InvalidParams("AES-KW data too short".to_string()));
            }
            let mut out = vec![0u8; data.len() - 8];
            kek.unwrap(data, &mut out)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            Ok(out)
        }
        32 => {
            let kek = Kek::<Aes256>::try_from(key)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            if data.len() < 8 {
                return Err(CryptoError::InvalidParams("AES-KW data too short".to_string()));
            }
            let mut out = vec![0u8; data.len() - 8];
            kek.unwrap(data, &mut out)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
            Ok(out)
        }
        _ => return Err(CryptoError::InvalidKeyLength),
    }?;
    Ok(output)
}

pub fn subtle_rsa_oaep_encrypt(
    hash: HashAlgorithm,
    key: &KeyInput,
    data: &[u8],
    label: Option<&[u8]>,
) -> Result<Vec<u8>, CryptoError> {
    let public_key = parse_rsa_public(key)?;
    let padding = match (hash, label) {
        (HashAlgorithm::Sha1, Some(label)) => {
            let label = String::from_utf8(label.to_vec())
                .map_err(|_| CryptoError::InvalidParams("RSA-OAEP label must be UTF-8".to_string()))?;
            Oaep::new_with_label::<Sha1, _>(label)
        }
        (HashAlgorithm::Sha1, None) => Oaep::new::<Sha1>(),
        (HashAlgorithm::Sha256, Some(label)) => {
            let label = String::from_utf8(label.to_vec())
                .map_err(|_| CryptoError::InvalidParams("RSA-OAEP label must be UTF-8".to_string()))?;
            Oaep::new_with_label::<Sha256, _>(label)
        }
        (HashAlgorithm::Sha256, None) => Oaep::new::<Sha256>(),
        (HashAlgorithm::Sha384, Some(label)) => {
            let label = String::from_utf8(label.to_vec())
                .map_err(|_| CryptoError::InvalidParams("RSA-OAEP label must be UTF-8".to_string()))?;
            Oaep::new_with_label::<Sha384, _>(label)
        }
        (HashAlgorithm::Sha384, None) => Oaep::new::<Sha384>(),
        (HashAlgorithm::Sha512, Some(label)) => {
            let label = String::from_utf8(label.to_vec())
                .map_err(|_| CryptoError::InvalidParams("RSA-OAEP label must be UTF-8".to_string()))?;
            Oaep::new_with_label::<Sha512, _>(label)
        }
        (HashAlgorithm::Sha512, None) => Oaep::new::<Sha512>(),
        _ => {
            return Err(CryptoError::UnsupportedAlgorithm(format!(
                "Unsupported RSA-OAEP hash {:?}",
                hash
            )))
        }
    };
    let mut rng = OsRng;
    public_key
        .encrypt(&mut rng, padding, data)
        .map_err(|e| CryptoError::InvalidParams(e.to_string()))
}

pub fn subtle_rsa_oaep_decrypt(
    hash: HashAlgorithm,
    key: &KeyInput,
    data: &[u8],
    label: Option<&[u8]>,
) -> Result<Vec<u8>, CryptoError> {
    let private_key = parse_rsa_private(key)?;
    let padding = match (hash, label) {
        (HashAlgorithm::Sha1, Some(label)) => {
            let label = String::from_utf8(label.to_vec())
                .map_err(|_| CryptoError::InvalidParams("RSA-OAEP label must be UTF-8".to_string()))?;
            Oaep::new_with_label::<Sha1, _>(label)
        }
        (HashAlgorithm::Sha1, None) => Oaep::new::<Sha1>(),
        (HashAlgorithm::Sha256, Some(label)) => {
            let label = String::from_utf8(label.to_vec())
                .map_err(|_| CryptoError::InvalidParams("RSA-OAEP label must be UTF-8".to_string()))?;
            Oaep::new_with_label::<Sha256, _>(label)
        }
        (HashAlgorithm::Sha256, None) => Oaep::new::<Sha256>(),
        (HashAlgorithm::Sha384, Some(label)) => {
            let label = String::from_utf8(label.to_vec())
                .map_err(|_| CryptoError::InvalidParams("RSA-OAEP label must be UTF-8".to_string()))?;
            Oaep::new_with_label::<Sha384, _>(label)
        }
        (HashAlgorithm::Sha384, None) => Oaep::new::<Sha384>(),
        (HashAlgorithm::Sha512, Some(label)) => {
            let label = String::from_utf8(label.to_vec())
                .map_err(|_| CryptoError::InvalidParams("RSA-OAEP label must be UTF-8".to_string()))?;
            Oaep::new_with_label::<Sha512, _>(label)
        }
        (HashAlgorithm::Sha512, None) => Oaep::new::<Sha512>(),
        _ => {
            return Err(CryptoError::UnsupportedAlgorithm(format!(
                "Unsupported RSA-OAEP hash {:?}",
                hash
            )))
        }
    };
    private_key
        .decrypt(padding, data)
        .map_err(|e| CryptoError::InvalidParams(e.to_string()))
}

pub fn subtle_derive_bits_ecdh(
    private_key: &KeyInput,
    public_key: &KeyInput,
) -> Result<Vec<u8>, CryptoError> {
    let secret = parse_p256_secret(private_key)?;
    let public = parse_p256_public_key(public_key)?;
    let shared = diffie_hellman(secret.to_nonzero_scalar(), public.as_affine());
    Ok(shared.raw_secret_bytes().to_vec())
}

pub fn subtle_derive_bits_hkdf(
    hash: HashAlgorithm,
    key: &[u8],
    salt: &[u8],
    info: &[u8],
    length: usize,
) -> Result<Vec<u8>, CryptoError> {
    let mut okm = vec![0u8; length];
    match hash {
        HashAlgorithm::Sha1 => {
            let hk = Hkdf::<Sha1>::new(Some(salt), key);
            hk.expand(info, &mut okm)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
        }
        HashAlgorithm::Sha256 => {
            let hk = Hkdf::<Sha256>::new(Some(salt), key);
            hk.expand(info, &mut okm)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
        }
        HashAlgorithm::Sha384 => {
            let hk = Hkdf::<Sha384>::new(Some(salt), key);
            hk.expand(info, &mut okm)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
        }
        HashAlgorithm::Sha512 => {
            let hk = Hkdf::<Sha512>::new(Some(salt), key);
            hk.expand(info, &mut okm)
                .map_err(|e| CryptoError::InvalidParams(e.to_string()))?;
        }
        _ => {
            return Err(CryptoError::UnsupportedAlgorithm(format!(
                "Unsupported HKDF hash {:?}",
                hash
            )))
        }
    }
    Ok(okm)
}

pub fn subtle_derive_bits_pbkdf2(
    hash: HashAlgorithm,
    key: &[u8],
    salt: &[u8],
    iterations: u32,
    length: usize,
) -> Result<Vec<u8>, CryptoError> {
    let mut out = vec![0u8; length];
    match hash {
        HashAlgorithm::Sha1 => pbkdf2_hmac::<Sha1>(key, salt, iterations, &mut out),
        HashAlgorithm::Sha256 => pbkdf2_hmac::<Sha256>(key, salt, iterations, &mut out),
        HashAlgorithm::Sha384 => pbkdf2_hmac::<Sha384>(key, salt, iterations, &mut out),
        HashAlgorithm::Sha512 => pbkdf2_hmac::<Sha512>(key, salt, iterations, &mut out),
        _ => {
            return Err(CryptoError::UnsupportedAlgorithm(format!(
                "Unsupported PBKDF2 hash {:?}",
                hash
            )))
        }
    }
    Ok(out)
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
