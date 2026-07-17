//! `node:crypto` native core — hashing (SHA-2 family), HMAC, and CSPRNG bytes.
//!
//! Public-key / cipher operations are out of scope for this slice; the focus is
//! the high-frequency `createHash`/`createHmac`/`randomBytes` surface. Bytes
//! cross the native/JS boundary as latin1 strings (the same bridge `fs` uses).
//! The CommonJS namespace is assembled inside one rooted native scope.

use otter_runtime::{
    CapabilitySet, RuntimeLocal as Local, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeNativeScope as NativeScope, RuntimeTaskSpawner,
    RuntimeValue as Value, runtime_arg_to_string,
};
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};

const SHIM: &str = include_str!("crypto.js");

/// CommonJS export: the `crypto` namespace built by `crypto.js`.
pub fn crypto_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    let native = native_value(scope)?;
    let buffer = crate::buffer::buffer_cjs_value(scope, caps, runtime_task_spawner.clone())?;
    let events = crate::events::events_cjs_value(scope, caps, runtime_task_spawner)?;
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:crypto",
        SHIM,
        &[
            ("__cryptonative", native),
            ("buffer", buffer),
            ("events", events),
        ],
    )
}

fn native_value<'scope>(scope: &mut NativeScope<'scope, '_>) -> Result<Local<'scope>, NativeError> {
    let object = scope.object()?;
    macro_rules! m {
        ($name:literal, $len:expr, $f:ident) => {
            let method = scope.native_method($name, $len, $f)?;
            scope.set(object, $name, method)?;
        };
    }

    m!("randomBytes", 1, random_bytes);
    m!("hashDigest", 2, hash_digest);
    m!("hmacDigest", 3, hmac_digest);

    Ok(object)
}

fn bytes_to_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn latin1_to_bytes(s: &str) -> Vec<u8> {
    s.chars().map(|c| c as u32 as u8).collect()
}

fn normalize_algo(algo: &str) -> String {
    algo.to_ascii_lowercase().replace('-', "")
}

/// `randomBytes(size)` — cryptographically secure random bytes as latin1.
fn random_bytes(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let size = args
        .first()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        .max(0.0) as usize;
    if size > 0x4000_0000 {
        return Err(crate::type_error(
            "crypto",
            "requested too many random bytes",
        ));
    }
    let mut buf = vec![0u8; size];
    getrandom::fill(&mut buf)
        .map_err(|e| crate::type_error("crypto", format!("randomBytes failed: {e}")))?;
    crate::string_value(ctx, &bytes_to_latin1(&buf))
}

/// `hashDigest(algorithm, dataLatin1)` — one-shot digest as latin1.
fn hash_digest(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let algo = normalize_algo(&runtime_arg_to_string(args, 0, ctx.heap()));
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 1, ctx.heap()));
    let digest = match algo.as_str() {
        "sha1" => sha1::Sha1::digest(&data).to_vec(),
        "md5" => md5::Md5::digest(&data).to_vec(),
        "sha224" => Sha224::digest(&data).to_vec(),
        "sha256" => Sha256::digest(&data).to_vec(),
        "sha384" => Sha384::digest(&data).to_vec(),
        "sha512" => Sha512::digest(&data).to_vec(),
        other => {
            return Err(NativeError::Coded {
                kind: otter_vm::ErrorKind::Error,
                code: "ERR_OSSL_EVP_UNSUPPORTED",
                message: format!("Digest method not supported: {other}"),
            });
        }
    };
    crate::string_value(ctx, &bytes_to_latin1(&digest))
}

/// `hmacDigest(algorithm, keyLatin1, dataLatin1)` — HMAC as latin1. Implemented
/// directly (RFC 2104) over the SHA-2 family so no extra crate is needed.
fn hmac_digest(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let algo = normalize_algo(&runtime_arg_to_string(args, 0, ctx.heap()));
    let key = latin1_to_bytes(&runtime_arg_to_string(args, 1, ctx.heap()));
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 2, ctx.heap()));
    let (block, digest): (usize, Vec<u8>) = match algo.as_str() {
        "sha1" => (64, hmac::<sha1::Sha1>(&key, &data, 64)),
        "md5" => (64, hmac::<md5::Md5>(&key, &data, 64)),
        "sha224" => (64, hmac::<Sha224>(&key, &data, 64)),
        "sha256" => (64, hmac::<Sha256>(&key, &data, 64)),
        "sha384" => (128, hmac::<Sha384>(&key, &data, 128)),
        "sha512" => (128, hmac::<Sha512>(&key, &data, 128)),
        other => {
            return Err(NativeError::Coded {
                kind: otter_vm::ErrorKind::Error,
                code: "ERR_OSSL_EVP_UNSUPPORTED",
                message: format!("Digest method not supported: {other}"),
            });
        }
    };
    let _ = block;
    crate::string_value(ctx, &bytes_to_latin1(&digest))
}

fn hmac<D: Digest>(key: &[u8], data: &[u8], block_size: usize) -> Vec<u8> {
    let mut block_key = vec![0u8; block_size];
    if key.len() > block_size {
        let hashed = D::digest(key);
        block_key[..hashed.len()].copy_from_slice(&hashed);
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = block_key.iter().map(|&b| b ^ 0x36).collect();
    let opad: Vec<u8> = block_key.iter().map(|&b| b ^ 0x5c).collect();
    let mut inner = D::new();
    inner.update(&ipad);
    inner.update(data);
    let inner_digest = inner.finalize();
    let mut outer = D::new();
    outer.update(&opad);
    outer.update(&inner_digest);
    outer.finalize().to_vec()
}
