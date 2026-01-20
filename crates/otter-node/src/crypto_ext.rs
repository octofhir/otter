//! Crypto extension module using the new architecture.
//!
//! This module provides the node:crypto extension for cryptographic operations.
//!
//! ## Architecture
//!
//! - `crypto.rs` - Rust crypto implementation
//! - `crypto_ext.rs` - Extension creation with ops
//! - `crypto.js` - JavaScript crypto wrapper (Hash, Hmac classes)
//!
//! Note: This module uses shared state for hash/hmac contexts which doesn't fit
//! the #[dive] pattern, so we use traditional op_sync with closures.

use otter_macros::dive;
use otter_runtime::Extension;
use otter_runtime::extension::{OpDecl, op_async, op_sync};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

use crate::crypto;

#[derive(Debug, Clone)]
struct BufferLike(Vec<u8>);

impl BufferLike {
    fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl<'de> Deserialize<'de> for BufferLike {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(s) = value.as_str() {
            return Ok(Self(s.as_bytes().to_vec()));
        }
        if let Some(arr) = value.as_array() {
            let bytes = arr
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect();
            return Ok(Self(bytes));
        }
        if let Some(obj) = value.as_object() {
            if let Some(data) = obj.get("data").and_then(|v| v.as_array()) {
                let bytes = data
                    .iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u8))
                    .collect();
                return Ok(Self(bytes));
            }
        }
        Err(serde::de::Error::custom("Invalid buffer-like value"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyInputArg {
    key: BufferLike,
    format: Option<String>,
    r#type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PaddingArg {
    Number(u32),
    Text(String),
}

impl PaddingArg {
    fn to_rsa_padding(&self) -> Option<crypto::RsaPadding> {
        match self {
            PaddingArg::Number(1) => Some(crypto::RsaPadding::Pkcs1),
            PaddingArg::Number(6) => Some(crypto::RsaPadding::Pss),
            PaddingArg::Text(text) => match text.to_lowercase().as_str() {
                "pkcs1" => Some(crypto::RsaPadding::Pkcs1),
                "pss" => Some(crypto::RsaPadding::Pss),
                _ => None,
            },
            _ => None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignOptionsArg {
    dsa_encoding: Option<String>,
    padding: Option<PaddingArg>,
    salt_length: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyEncodingArg {
    format: Option<String>,
    r#type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyPairOptionsArg {
    modulus_length: Option<usize>,
    public_exponent: Option<u64>,
    named_curve: Option<String>,
    public_key_encoding: Option<KeyEncodingArg>,
    private_key_encoding: Option<KeyEncodingArg>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtleAesGcmOptionsArg {
    iv: BufferLike,
    additional_data: Option<BufferLike>,
    tag_length: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtleAesCbcOptionsArg {
    iv: BufferLike,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtleAesCtrOptionsArg {
    counter: BufferLike,
    length: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtleRsaOaepOptionsArg {
    hash: String,
    label: Option<BufferLike>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtleHkdfOptionsArg {
    hash: String,
    salt: BufferLike,
    info: BufferLike,
    length: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtlePbkdf2OptionsArg {
    hash: String,
    salt: BufferLike,
    iterations: u32,
    length: u32,
}

#[derive(Serialize)]
struct BufferJson {
    r#type: String,
    data: Vec<u8>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum KeyOutputJson {
    Pem(String),
    Buffer(BufferJson),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyPairJson {
    public_key: KeyOutputJson,
    private_key: KeyOutputJson,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyMaterialJson {
    r#type: String,
    data: Vec<u8>,
    key_type: String,
}

fn normalize_key_input(
    value: KeyInputArg,
) -> Result<crypto::KeyInput, otter_runtime::error::JscError> {
    let format = match value.format.as_deref() {
        Some(fmt) => crypto::KeyFormat::parse(fmt)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?,
        None => {
            if value.key.0.starts_with(b"-----BEGIN") {
                crypto::KeyFormat::Pem
            } else {
                crypto::KeyFormat::Der
            }
        }
    };
    let key_type = match value.r#type.as_deref() {
        Some(t) => Some(
            crypto::KeyType::parse(t)
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?,
        ),
        None => None,
    };
    Ok(crypto::KeyInput {
        data: value.key.into_vec(),
        format,
        key_type,
    })
}

fn normalize_sign_options(
    options: Option<SignOptionsArg>,
) -> Result<crypto::SignOptions, otter_runtime::error::JscError> {
    let opts = options.unwrap_or_default();
    let dsa_encoding = match opts.dsa_encoding.as_deref() {
        Some(value) => Some(
            crypto::DsaEncoding::parse(value)
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?,
        ),
        None => None,
    };
    let padding = opts.padding.and_then(|p| p.to_rsa_padding());
    Ok(crypto::SignOptions {
        dsa_encoding,
        padding,
        salt_length: opts.salt_length.map(|v| v as usize),
    })
}

fn encode_key_output(output: crypto::KeyOutput) -> KeyOutputJson {
    match output {
        crypto::KeyOutput::Pem(value) => KeyOutputJson::Pem(value),
        crypto::KeyOutput::Der(data) => KeyOutputJson::Buffer(BufferJson {
            r#type: "Buffer".to_string(),
            data,
        }),
    }
}

fn normalize_key_pair_options(
    key_type: String,
    options: KeyPairOptionsArg,
) -> Result<crypto::KeyPairOptions, otter_runtime::error::JscError> {
    let public_encoding = options.public_key_encoding.unwrap_or(KeyEncodingArg {
        format: Some("pem".to_string()),
        r#type: Some("spki".to_string()),
    });
    let private_encoding = options.private_key_encoding.unwrap_or(KeyEncodingArg {
        format: Some("pem".to_string()),
        r#type: Some("pkcs8".to_string()),
    });

    let public_key_format =
        crypto::KeyFormat::parse(public_encoding.format.as_deref().unwrap_or("pem"))
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
    let public_key_type =
        crypto::KeyType::parse(public_encoding.r#type.as_deref().unwrap_or("spki"))
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
    let private_key_format =
        crypto::KeyFormat::parse(private_encoding.format.as_deref().unwrap_or("pem"))
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
    let private_key_type =
        crypto::KeyType::parse(private_encoding.r#type.as_deref().unwrap_or("pkcs8"))
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

    Ok(crypto::KeyPairOptions {
        key_type,
        modulus_length: options.modulus_length,
        public_exponent: options.public_exponent,
        named_curve: options.named_curve,
        public_key_format,
        public_key_type,
        private_key_format,
        private_key_type,
    })
}

fn normalize_aes_gcm_options(options: SubtleAesGcmOptionsArg) -> crypto::SubtleAesGcmOptions {
    crypto::SubtleAesGcmOptions {
        iv: options.iv.into_vec(),
        additional_data: options.additional_data.map(BufferLike::into_vec),
        tag_length: options.tag_length,
    }
}

fn normalize_aes_cbc_options(options: SubtleAesCbcOptionsArg) -> Vec<u8> {
    options.iv.into_vec()
}

fn normalize_aes_ctr_options(options: SubtleAesCtrOptionsArg) -> (Vec<u8>, u32) {
    (options.counter.into_vec(), options.length)
}

fn bits_to_len(bits: u32) -> Result<usize, otter_runtime::error::JscError> {
    if bits % 8 != 0 {
        return Err(otter_runtime::error::JscError::internal(
            "bit length must be a multiple of 8",
        ));
    }
    Ok((bits / 8) as usize)
}

fn key_type_to_string(key_type: crypto::KeyType) -> String {
    match key_type {
        crypto::KeyType::Pkcs1 => "pkcs1",
        crypto::KeyType::Pkcs8 => "pkcs8",
        crypto::KeyType::Spki => "spki",
        crypto::KeyType::Sec1 => "sec1",
    }
    .to_string()
}

// ============================================================================
// Dive-based ops
// ============================================================================

#[dive(swift)]
fn crypto_sign(
    algorithm: String,
    key: KeyInputArg,
    data: BufferLike,
    options: Option<SignOptionsArg>,
) -> Result<BufferJson, crypto::CryptoError> {
    let key =
        normalize_key_input(key).map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let opts = normalize_sign_options(options)
        .map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let signature = crypto::sign(&algorithm, &key, &data.into_vec(), &opts)?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: signature,
    })
}

#[dive(swift)]
fn crypto_verify(
    algorithm: String,
    key: KeyInputArg,
    data: BufferLike,
    signature: BufferLike,
    options: Option<SignOptionsArg>,
) -> Result<bool, crypto::CryptoError> {
    let key =
        normalize_key_input(key).map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let opts = normalize_sign_options(options)
        .map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    crypto::verify(
        &algorithm,
        &key,
        &data.into_vec(),
        &signature.into_vec(),
        &opts,
    )
}

#[dive(swift)]
fn crypto_generate_key_pair_sync(
    key_type: String,
    options: KeyPairOptionsArg,
) -> Result<KeyPairJson, crypto::CryptoError> {
    let opts = normalize_key_pair_options(key_type, options)
        .map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let output = crypto::generate_key_pair(&opts)?;
    Ok(KeyPairJson {
        public_key: encode_key_output(output.public_key),
        private_key: encode_key_output(output.private_key),
    })
}

#[dive(deep)]
async fn crypto_generate_key_pair(
    key_type: String,
    options: KeyPairOptionsArg,
) -> Result<KeyPairJson, crypto::CryptoError> {
    let opts = normalize_key_pair_options(key_type, options)
        .map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let output = tokio::task::spawn_blocking(move || crypto::generate_key_pair(&opts))
        .await
        .map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))??;
    Ok(KeyPairJson {
        public_key: encode_key_output(output.public_key),
        private_key: encode_key_output(output.private_key),
    })
}

#[dive(swift)]
fn crypto_subtle_digest(
    algorithm: String,
    data: BufferLike,
) -> Result<BufferJson, crypto::CryptoError> {
    let digest = crypto::subtle_digest(&algorithm, &data.into_vec())?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: digest,
    })
}

#[dive(swift)]
fn crypto_subtle_encrypt_aes_gcm(
    key: BufferLike,
    data: BufferLike,
    options: SubtleAesGcmOptionsArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let opts = normalize_aes_gcm_options(options);
    let output = crypto::subtle_encrypt_aes_gcm(&key.into_vec(), &data.into_vec(), &opts)?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_decrypt_aes_gcm(
    key: BufferLike,
    data: BufferLike,
    options: SubtleAesGcmOptionsArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let opts = normalize_aes_gcm_options(options);
    let output = crypto::subtle_decrypt_aes_gcm(&key.into_vec(), &data.into_vec(), &opts)?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_encrypt_aes_cbc(
    key: BufferLike,
    data: BufferLike,
    options: SubtleAesCbcOptionsArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let iv = normalize_aes_cbc_options(options);
    let output = crypto::subtle_encrypt_aes_cbc(&key.into_vec(), &data.into_vec(), &iv)?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_decrypt_aes_cbc(
    key: BufferLike,
    data: BufferLike,
    options: SubtleAesCbcOptionsArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let iv = normalize_aes_cbc_options(options);
    let output = crypto::subtle_decrypt_aes_cbc(&key.into_vec(), &data.into_vec(), &iv)?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_encrypt_aes_ctr(
    key: BufferLike,
    data: BufferLike,
    options: SubtleAesCtrOptionsArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let (counter, length) = normalize_aes_ctr_options(options);
    let output =
        crypto::subtle_encrypt_aes_ctr(&key.into_vec(), &data.into_vec(), &counter, length)?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_decrypt_aes_ctr(
    key: BufferLike,
    data: BufferLike,
    options: SubtleAesCtrOptionsArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let (counter, length) = normalize_aes_ctr_options(options);
    let output =
        crypto::subtle_decrypt_aes_ctr(&key.into_vec(), &data.into_vec(), &counter, length)?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_wrap_aes_kw(
    key: BufferLike,
    data: BufferLike,
) -> Result<BufferJson, crypto::CryptoError> {
    let output = crypto::subtle_wrap_aes_kw(&key.into_vec(), &data.into_vec())?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_unwrap_aes_kw(
    key: BufferLike,
    data: BufferLike,
) -> Result<BufferJson, crypto::CryptoError> {
    let output = crypto::subtle_unwrap_aes_kw(&key.into_vec(), &data.into_vec())?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_rsa_oaep_encrypt(
    key: KeyInputArg,
    data: BufferLike,
    options: SubtleRsaOaepOptionsArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let key =
        normalize_key_input(key).map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let hash = crypto::HashAlgorithm::parse(&options.hash)?;
    let label = options.label.map(|v| v.into_vec());
    let output = crypto::subtle_rsa_oaep_encrypt(hash, &key, &data.into_vec(), label.as_deref())?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_rsa_oaep_decrypt(
    key: KeyInputArg,
    data: BufferLike,
    options: SubtleRsaOaepOptionsArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let key =
        normalize_key_input(key).map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let hash = crypto::HashAlgorithm::parse(&options.hash)?;
    let label = options.label.map(|v| v.into_vec());
    let output = crypto::subtle_rsa_oaep_decrypt(hash, &key, &data.into_vec(), label.as_deref())?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_derive_bits_ecdh(
    private_key: KeyInputArg,
    public_key: KeyInputArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let private_key = normalize_key_input(private_key)
        .map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let public_key = normalize_key_input(public_key)
        .map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let output = crypto::subtle_derive_bits_ecdh(&private_key, &public_key)?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_derive_bits_hkdf(
    key: BufferLike,
    options: SubtleHkdfOptionsArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let length = bits_to_len(options.length)
        .map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let hash = crypto::HashAlgorithm::parse(&options.hash)?;
    let output = crypto::subtle_derive_bits_hkdf(
        hash,
        &key.into_vec(),
        &options.salt.into_vec(),
        &options.info.into_vec(),
        length,
    )?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_subtle_derive_bits_pbkdf2(
    key: BufferLike,
    options: SubtlePbkdf2OptionsArg,
) -> Result<BufferJson, crypto::CryptoError> {
    let length = bits_to_len(options.length)
        .map_err(|e| crypto::CryptoError::InvalidParams(e.to_string()))?;
    let hash = crypto::HashAlgorithm::parse(&options.hash)?;
    let output = crypto::subtle_derive_bits_pbkdf2(
        hash,
        &key.into_vec(),
        &options.salt.into_vec(),
        options.iterations,
        length,
    )?;
    Ok(BufferJson {
        r#type: "Buffer".to_string(),
        data: output,
    })
}

#[dive(swift)]
fn crypto_jwk_to_der(jwk: crypto::JwkKey) -> Result<KeyMaterialJson, crypto::CryptoError> {
    let (data, key_type) = crypto::jwk_to_der(&jwk)?;
    Ok(KeyMaterialJson {
        r#type: "Buffer".to_string(),
        data,
        key_type: key_type_to_string(key_type),
    })
}

#[dive(swift)]
fn crypto_der_to_jwk(
    algorithm: String,
    key_type: String,
    key: BufferLike,
) -> Result<crypto::JwkKey, crypto::CryptoError> {
    let key_type = crypto::KeyType::parse(&key_type)?;
    crypto::der_to_jwk(&key.into_vec(), key_type, &algorithm)
}

/// Create the crypto extension.
///
/// This extension provides Node.js-compatible cryptographic functionality:
/// - randomBytes, randomUUID, getRandomValues
/// - createHash, createHmac (with incremental update/digest)
/// - hash (one-shot convenience)
pub fn extension() -> Extension {
    // Shared state for incremental hashing
    let hash_contexts: Arc<Mutex<HashMap<u32, crypto::Hash>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let hmac_contexts: Arc<Mutex<HashMap<u32, crypto::Hmac>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let cipher_contexts: Arc<Mutex<HashMap<u32, crypto::CipherContext>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let decipher_contexts: Arc<Mutex<HashMap<u32, crypto::CipherContext>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(std::sync::atomic::AtomicU32::new(1));

    let mut ops: Vec<OpDecl> = Vec::new();

    fn value_to_bytes(
        value: &serde_json::Value,
    ) -> Result<Vec<u8>, otter_runtime::error::JscError> {
        if let Some(s) = value.as_str() {
            Ok(s.as_bytes().to_vec())
        } else if let Some(arr) = value.as_array() {
            Ok(arr
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect())
        } else if let Some(obj) = value.as_object() {
            obj.get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .ok_or_else(|| {
                    otter_runtime::error::JscError::internal("Invalid Buffer-like object")
                })
        } else {
            Err(otter_runtime::error::JscError::internal(
                "Invalid data type",
            ))
        }
    }

    fn parse_scrypt_options(
        options: Option<&serde_json::Value>,
    ) -> Result<(u64, u32, u32), otter_runtime::error::JscError> {
        let mut n: u64 = 16384;
        let mut r: u32 = 8;
        let mut p: u32 = 1;

        if let Some(value) = options {
            if let Some(obj) = value.as_object() {
                if let Some(v) = obj.get("N").and_then(|v| v.as_u64()) {
                    n = v;
                }
                if let Some(v) = obj.get("r").and_then(|v| v.as_u64()) {
                    r = v as u32;
                }
                if let Some(v) = obj.get("p").and_then(|v| v.as_u64()) {
                    p = v as u32;
                }
            }
        }

        Ok((n, r, p))
    }

    // randomBytes(size) -> Buffer
    ops.push(op_sync("randomBytes", |_ctx, args| {
        let size = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let bytes = crypto::random_bytes(size)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!({
            "type": "Buffer",
            "data": bytes,
        }))
    }));

    // randomUUID() -> string
    ops.push(op_sync("randomUUID", |_ctx, _args| {
        Ok(json!(crypto::random_uuid()))
    }));

    // getRandomValues(length) -> array of random bytes
    ops.push(op_sync("getRandomValues", |_ctx, args| {
        let length = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let bytes = crypto::random_bytes(length)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!(bytes))
    }));

    ops.push(op_sync("getHashes", |_ctx, _args| {
        Ok(json!(crypto::get_hashes()))
    }));
    ops.push(op_sync("getCiphers", |_ctx, _args| {
        Ok(json!(crypto::get_ciphers()))
    }));
    ops.push(op_sync("getCurves", |_ctx, _args| {
        Ok(json!(crypto::get_curves()))
    }));

    ops.push(crypto_sign_dive_decl());
    ops.push(crypto_verify_dive_decl());
    ops.push(crypto_generate_key_pair_sync_dive_decl());
    ops.push(crypto_generate_key_pair_dive_decl());
    ops.push(crypto_subtle_digest_dive_decl());
    ops.push(crypto_subtle_encrypt_aes_gcm_dive_decl());
    ops.push(crypto_subtle_decrypt_aes_gcm_dive_decl());
    ops.push(crypto_subtle_encrypt_aes_cbc_dive_decl());
    ops.push(crypto_subtle_decrypt_aes_cbc_dive_decl());
    ops.push(crypto_subtle_encrypt_aes_ctr_dive_decl());
    ops.push(crypto_subtle_decrypt_aes_ctr_dive_decl());
    ops.push(crypto_subtle_wrap_aes_kw_dive_decl());
    ops.push(crypto_subtle_unwrap_aes_kw_dive_decl());
    ops.push(crypto_subtle_rsa_oaep_encrypt_dive_decl());
    ops.push(crypto_subtle_rsa_oaep_decrypt_dive_decl());
    ops.push(crypto_subtle_derive_bits_ecdh_dive_decl());
    ops.push(crypto_subtle_derive_bits_hkdf_dive_decl());
    ops.push(crypto_subtle_derive_bits_pbkdf2_dive_decl());
    ops.push(crypto_jwk_to_der_dive_decl());
    ops.push(crypto_der_to_jwk_dive_decl());

    // createHash(algorithm) -> hash_id
    let hash_ctx = hash_contexts.clone();
    let hash_id = next_id.clone();
    ops.push(op_sync("createHash", move |_ctx, args| {
        let algorithm = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("createHash requires algorithm")
        })?;

        let hash = crypto::create_hash(algorithm)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        let id = hash_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        hash_ctx.lock().insert(id, hash);

        Ok(json!(id))
    }));

    // hashUpdate(id, data) -> null
    let hash_ctx_update = hash_contexts.clone();
    ops.push(op_sync("hashUpdate", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("hashUpdate requires id"))?
            as u32;

        let data = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("hashUpdate requires data"))?;

        let bytes: Vec<u8> = if let Some(s) = data.as_str() {
            s.as_bytes().to_vec()
        } else if let Some(arr) = data.as_array() {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect()
        } else if let Some(obj) = data.as_object() {
            obj.get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut contexts = hash_ctx_update.lock();
        let hash = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid hash id"))?;

        hash.update(&bytes);
        Ok(json!(null))
    }));

    // hashDigest(id, encoding) -> string or Buffer
    let hash_ctx_digest = hash_contexts.clone();
    ops.push(op_sync("hashDigest", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("hashDigest requires id"))?
            as u32;

        let encoding = args.get(1).and_then(|v| v.as_str());

        let mut contexts = hash_ctx_digest.lock();
        let hash = contexts
            .remove(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid hash id"))?;

        let digest = hash.digest();

        match encoding {
            Some("hex") => Ok(json!(crypto::to_hex(&digest))),
            Some("base64") => Ok(json!(crypto::to_base64(&digest))),
            _ => Ok(json!({
                "type": "Buffer",
                "data": digest,
            })),
        }
    }));

    // createHmac(algorithm, key) -> hmac_id
    let hmac_ctx = hmac_contexts.clone();
    let hmac_id = next_id.clone();
    ops.push(op_sync("createHmac", move |_ctx, args| {
        let algorithm = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("createHmac requires algorithm")
        })?;

        let key_arg = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("createHmac requires key"))?;

        let key: Vec<u8> = if let Some(s) = key_arg.as_str() {
            s.as_bytes().to_vec()
        } else if let Some(arr) = key_arg.as_array() {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect()
        } else if let Some(obj) = key_arg.as_object() {
            obj.get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let hmac = crypto::create_hmac(algorithm, &key)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        let id = hmac_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        hmac_ctx.lock().insert(id, hmac);

        Ok(json!(id))
    }));

    // hmacUpdate(id, data) -> null
    let hmac_ctx_update = hmac_contexts.clone();
    ops.push(op_sync("hmacUpdate", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("hmacUpdate requires id"))?
            as u32;

        let data = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("hmacUpdate requires data"))?;

        let bytes: Vec<u8> = if let Some(s) = data.as_str() {
            s.as_bytes().to_vec()
        } else if let Some(arr) = data.as_array() {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect()
        } else if let Some(obj) = data.as_object() {
            obj.get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut contexts = hmac_ctx_update.lock();
        let hmac = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid hmac id"))?;

        hmac.update(&bytes);
        Ok(json!(null))
    }));

    // hmacDigest(id, encoding) -> string or Buffer
    let hmac_ctx_digest = hmac_contexts.clone();
    ops.push(op_sync("hmacDigest", move |_ctx, args| {
        let id = args
            .first()
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("hmacDigest requires id"))?
            as u32;

        let encoding = args.get(1).and_then(|v| v.as_str());

        let mut contexts = hmac_ctx_digest.lock();
        let hmac = contexts
            .remove(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid hmac id"))?;

        let digest = hmac.digest();

        match encoding {
            Some("hex") => Ok(json!(crypto::to_hex(&digest))),
            Some("base64") => Ok(json!(crypto::to_base64(&digest))),
            _ => Ok(json!({
                "type": "Buffer",
                "data": digest,
            })),
        }
    }));

    // hash(algorithm, data, encoding) -> one-shot hash
    ops.push(op_sync("hash", |_ctx, args| {
        let algorithm = args
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| otter_runtime::error::JscError::internal("hash requires algorithm"))?;

        let data = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("hash requires data"))?;

        let encoding = args.get(2).and_then(|v| v.as_str());

        let bytes: Vec<u8> = if let Some(s) = data.as_str() {
            s.as_bytes().to_vec()
        } else if let Some(arr) = data.as_array() {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect()
        } else if let Some(obj) = data.as_object() {
            obj.get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let digest = crypto::hash(algorithm, &bytes)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        match encoding {
            Some("hex") => Ok(json!(crypto::to_hex(&digest))),
            Some("base64") => Ok(json!(crypto::to_base64(&digest))),
            _ => Ok(json!({
                "type": "Buffer",
                "data": digest,
            })),
        }
    }));

    // timingSafeEqual(a, b) -> bool
    ops.push(op_sync("timingSafeEqual", |_ctx, args| {
        let a = args.first().ok_or_else(|| {
            otter_runtime::error::JscError::internal("timingSafeEqual requires a")
        })?;
        let b = args.get(1).ok_or_else(|| {
            otter_runtime::error::JscError::internal("timingSafeEqual requires b")
        })?;

        let a_bytes = value_to_bytes(a)?;
        let b_bytes = value_to_bytes(b)?;

        let equal = crypto::timing_safe_equal(&a_bytes, &b_bytes)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        Ok(json!(equal))
    }));

    // pbkdf2Sync(password, salt, iterations, keylen, digest) -> Buffer
    ops.push(op_sync("pbkdf2Sync", |_ctx, args| {
        let password = args.first().ok_or_else(|| {
            otter_runtime::error::JscError::internal("pbkdf2Sync requires password")
        })?;
        let salt = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("pbkdf2Sync requires salt"))?;
        let iterations = args.get(2).and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("pbkdf2Sync requires iterations")
        })? as u32;
        let key_len =
            args.get(3).and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("pbkdf2Sync requires keylen")
            })? as usize;
        let digest = args.get(4).and_then(|v| v.as_str()).unwrap_or("sha1");

        let out = crypto::pbkdf2(
            &value_to_bytes(password)?,
            &value_to_bytes(salt)?,
            iterations,
            key_len,
            digest,
        )
        .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!({
            "type": "Buffer",
            "data": out,
        }))
    }));

    // pbkdf2(password, salt, iterations, keylen, digest) -> Promise<Buffer>
    ops.push(op_async("pbkdf2", |_ctx, args| async move {
        let password = args
            .first()
            .ok_or_else(|| otter_runtime::error::JscError::internal("pbkdf2 requires password"))?;
        let salt = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("pbkdf2 requires salt"))?;
        let iterations =
            args.get(2).and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("pbkdf2 requires iterations")
            })? as u32;
        let key_len = args
            .get(3)
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("pbkdf2 requires keylen"))?
            as usize;
        let digest = args
            .get(4)
            .and_then(|v| v.as_str())
            .unwrap_or("sha1")
            .to_string();

        let password_bytes = value_to_bytes(password)?;
        let salt_bytes = value_to_bytes(salt)?;

        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle
                .spawn_blocking(move || {
                    crypto::pbkdf2(&password_bytes, &salt_bytes, iterations, key_len, &digest)
                })
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?,
            Err(_) => crypto::pbkdf2(&password_bytes, &salt_bytes, iterations, key_len, &digest),
        }
        .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!({
            "type": "Buffer",
            "data": result,
        }))
    }));

    // scryptSync(password, salt, keylen, options) -> Buffer
    ops.push(op_sync("scryptSync", |_ctx, args| {
        let password = args.first().ok_or_else(|| {
            otter_runtime::error::JscError::internal("scryptSync requires password")
        })?;
        let salt = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("scryptSync requires salt"))?;
        let key_len =
            args.get(2).and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("scryptSync requires keylen")
            })? as usize;
        let options = args.get(3);
        let (n, r, p) = parse_scrypt_options(options)?;

        let out = crypto::scrypt(
            &value_to_bytes(password)?,
            &value_to_bytes(salt)?,
            key_len,
            n,
            r,
            p,
        )
        .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!({
            "type": "Buffer",
            "data": out,
        }))
    }));

    // scrypt(password, salt, keylen, options) -> Promise<Buffer>
    ops.push(op_async("scrypt", |_ctx, args| async move {
        let password = args
            .first()
            .ok_or_else(|| otter_runtime::error::JscError::internal("scrypt requires password"))?;
        let salt = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("scrypt requires salt"))?;
        let key_len = args
            .get(2)
            .and_then(|v| v.as_u64())
            .ok_or_else(|| otter_runtime::error::JscError::internal("scrypt requires keylen"))?
            as usize;
        let options = args.get(3);
        let (n, r, p) = parse_scrypt_options(options)?;

        let password_bytes = value_to_bytes(password)?;
        let salt_bytes = value_to_bytes(salt)?;

        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle
                .spawn_blocking(move || {
                    crypto::scrypt(&password_bytes, &salt_bytes, key_len, n, r, p)
                })
                .await
                .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?,
            Err(_) => crypto::scrypt(&password_bytes, &salt_bytes, key_len, n, r, p),
        }
        .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!({
            "type": "Buffer",
            "data": result,
        }))
    }));

    fn parse_auth_tag_len(options: Option<&serde_json::Value>) -> Option<usize> {
        options
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("authTagLength"))
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
    }

    // createCipheriv(algorithm, key, iv, options) -> id
    let cipher_ctx_create = cipher_contexts.clone();
    let cipher_id = next_id.clone();
    ops.push(op_sync("createCipheriv", move |_ctx, args| {
        let algorithm = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("createCipheriv requires algorithm")
        })?;
        let key = args.get(1).ok_or_else(|| {
            otter_runtime::error::JscError::internal("createCipheriv requires key")
        })?;
        let iv = args.get(2).ok_or_else(|| {
            otter_runtime::error::JscError::internal("createCipheriv requires iv")
        })?;
        let options = args.get(3);

        let alg = crypto::CipherAlgorithm::parse(algorithm)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        let auth_tag_len = parse_auth_tag_len(options);
        let ctx = crypto::CipherContext::new(
            alg,
            &value_to_bytes(key)?,
            &value_to_bytes(iv)?,
            true,
            auth_tag_len,
        )
        .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        let id = cipher_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        cipher_ctx_create.lock().insert(id, ctx);
        Ok(json!(id))
    }));

    // createDecipheriv(algorithm, key, iv, options) -> id
    let decipher_ctx_create = decipher_contexts.clone();
    let decipher_id = next_id.clone();
    ops.push(op_sync("createDecipheriv", move |_ctx, args| {
        let algorithm = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("createDecipheriv requires algorithm")
        })?;
        let key = args.get(1).ok_or_else(|| {
            otter_runtime::error::JscError::internal("createDecipheriv requires key")
        })?;
        let iv = args.get(2).ok_or_else(|| {
            otter_runtime::error::JscError::internal("createDecipheriv requires iv")
        })?;
        let options = args.get(3);

        let alg = crypto::CipherAlgorithm::parse(algorithm)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        let auth_tag_len = parse_auth_tag_len(options);
        let ctx = crypto::CipherContext::new(
            alg,
            &value_to_bytes(key)?,
            &value_to_bytes(iv)?,
            false,
            auth_tag_len,
        )
        .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        let id = decipher_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        decipher_ctx_create.lock().insert(id, ctx);
        Ok(json!(id))
    }));

    // cipherUpdate(id, data) -> Buffer
    let cipher_ctx_update = cipher_contexts.clone();
    ops.push(op_sync("cipherUpdate", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("cipherUpdate requires id")
            })? as u32;
        let data = args.get(1).ok_or_else(|| {
            otter_runtime::error::JscError::internal("cipherUpdate requires data")
        })?;

        let mut contexts = cipher_ctx_update.lock();
        let ctx = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid cipher id"))?;
        let out = ctx
            .update(&value_to_bytes(data)?)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!({
            "type": "Buffer",
            "data": out,
        }))
    }));

    // cipherFinal(id) -> Buffer
    let cipher_ctx_final = cipher_contexts.clone();
    ops.push(op_sync("cipherFinal", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("cipherFinal requires id")
            })? as u32;

        let mut contexts = cipher_ctx_final.lock();
        let mut ctx = contexts
            .remove(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid cipher id"))?;
        let out = ctx
            .finalize()
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        let tag = ctx.get_auth_tag();

        Ok(json!({
            "type": "Buffer",
            "data": out,
            "authTag": tag.map(|t| json!({ "type": "Buffer", "data": t })),
        }))
    }));

    // cipherSetAAD(id, aad)
    let cipher_ctx_aad = cipher_contexts.clone();
    ops.push(op_sync("cipherSetAAD", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("cipherSetAAD requires id")
            })? as u32;
        let aad = args
            .get(1)
            .ok_or_else(|| otter_runtime::error::JscError::internal("cipherSetAAD requires aad"))?;

        let mut contexts = cipher_ctx_aad.lock();
        let ctx = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid cipher id"))?;
        ctx.set_aad(&value_to_bytes(aad)?)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        Ok(json!(null))
    }));

    // cipherSetAutoPadding(id, value)
    let cipher_ctx_pad = cipher_contexts.clone();
    ops.push(op_sync("cipherSetAutoPadding", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("cipherSetAutoPadding requires id")
        })? as u32;
        let value = args.get(1).and_then(|v| v.as_bool()).unwrap_or(true);

        let mut contexts = cipher_ctx_pad.lock();
        let ctx = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid cipher id"))?;
        ctx.set_auto_padding(value)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        Ok(json!(true))
    }));

    // cipherGetAuthTag(id) -> Buffer
    let cipher_ctx_tag = cipher_contexts.clone();
    ops.push(op_sync("cipherGetAuthTag", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("cipherGetAuthTag requires id")
        })? as u32;
        let contexts = cipher_ctx_tag.lock();
        let ctx = contexts
            .get(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid cipher id"))?;
        let tag = ctx
            .get_auth_tag()
            .ok_or_else(|| otter_runtime::error::JscError::internal("Auth tag not available"))?;
        Ok(json!({
            "type": "Buffer",
            "data": tag,
        }))
    }));

    // decipherUpdate(id, data) -> Buffer
    let decipher_ctx_update = decipher_contexts.clone();
    ops.push(op_sync("decipherUpdate", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("decipherUpdate requires id")
            })? as u32;
        let data = args.get(1).ok_or_else(|| {
            otter_runtime::error::JscError::internal("decipherUpdate requires data")
        })?;

        let mut contexts = decipher_ctx_update.lock();
        let ctx = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid decipher id"))?;
        let out = ctx
            .update(&value_to_bytes(data)?)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!({
            "type": "Buffer",
            "data": out,
        }))
    }));

    // decipherFinal(id) -> Buffer
    let decipher_ctx_final = decipher_contexts.clone();
    ops.push(op_sync("decipherFinal", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("decipherFinal requires id")
            })? as u32;

        let mut contexts = decipher_ctx_final.lock();
        let mut ctx = contexts
            .remove(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid decipher id"))?;
        let out = ctx
            .finalize()
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;

        Ok(json!({
            "type": "Buffer",
            "data": out,
        }))
    }));

    // decipherSetAAD(id, aad)
    let decipher_ctx_aad = decipher_contexts.clone();
    ops.push(op_sync("decipherSetAAD", move |_ctx, args| {
        let id =
            args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
                otter_runtime::error::JscError::internal("decipherSetAAD requires id")
            })? as u32;
        let aad = args.get(1).ok_or_else(|| {
            otter_runtime::error::JscError::internal("decipherSetAAD requires aad")
        })?;

        let mut contexts = decipher_ctx_aad.lock();
        let ctx = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid decipher id"))?;
        ctx.set_aad(&value_to_bytes(aad)?)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        Ok(json!(null))
    }));

    // decipherSetAuthTag(id, tag)
    let decipher_ctx_tag = decipher_contexts.clone();
    ops.push(op_sync("decipherSetAuthTag", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("decipherSetAuthTag requires id")
        })? as u32;
        let tag = args.get(1).ok_or_else(|| {
            otter_runtime::error::JscError::internal("decipherSetAuthTag requires tag")
        })?;

        let mut contexts = decipher_ctx_tag.lock();
        let ctx = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid decipher id"))?;
        ctx.set_auth_tag(&value_to_bytes(tag)?)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        Ok(json!(null))
    }));

    // decipherSetAutoPadding(id, value)
    let decipher_ctx_pad = decipher_contexts.clone();
    ops.push(op_sync("decipherSetAutoPadding", move |_ctx, args| {
        let id = args.first().and_then(|v| v.as_u64()).ok_or_else(|| {
            otter_runtime::error::JscError::internal("decipherSetAutoPadding requires id")
        })? as u32;
        let value = args.get(1).and_then(|v| v.as_bool()).unwrap_or(true);

        let mut contexts = decipher_ctx_pad.lock();
        let ctx = contexts
            .get_mut(&id)
            .ok_or_else(|| otter_runtime::error::JscError::internal("Invalid decipher id"))?;
        ctx.set_auto_padding(value)
            .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))?;
        Ok(json!(true))
    }));

    Extension::new("crypto")
        .with_ops(ops)
        .with_js(include_str!("crypto.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "crypto");
        assert!(ext.js_code().is_some());
    }
}
