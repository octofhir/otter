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

use parking_lot::Mutex;
use otter_runtime::extension::{op_sync, OpDecl};
use otter_runtime::Extension;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

use crate::crypto;

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
    let next_id = Arc::new(std::sync::atomic::AtomicU32::new(1));

    let mut ops: Vec<OpDecl> = Vec::new();

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
