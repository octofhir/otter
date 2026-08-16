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
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:crypto", SHIM, module, require)
}

/// Hidden CommonJS row supplying the pure native crypto primitives.
pub fn crypto_native_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    _require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    native_value(scope)
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
    m!("pbkdf2Digest", 5, pbkdf2_digest);
    m!("hkdfDerive", 5, hkdf_derive);

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

/// `pbkdf2(password, salt, iterations, keylen, digest)` — RFC 2898 PBKDF2 over
/// the same HMAC this module already builds, so every digest it supports is
/// available here too.
fn pbkdf2_digest(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let algo = normalize_algo(&runtime_arg_to_string(args, 0, ctx.heap()));
    let password = latin1_to_bytes(&runtime_arg_to_string(args, 1, ctx.heap()));
    let salt = latin1_to_bytes(&runtime_arg_to_string(args, 2, ctx.heap()));
    let iterations = number_arg(args, 3).max(1.0) as u32;
    let key_len = number_arg(args, 4).max(0.0) as usize;

    let derived = with_digest(&algo, |block, mac| {
        pbkdf2(mac, block, &password, &salt, iterations, key_len)
    })?;
    crate::string_value(ctx, &bytes_to_latin1(&derived))
}

/// `hkdf(digest, ikm, salt, info, keylen)` — RFC 5869 extract-then-expand.
fn hkdf_derive(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let algo = normalize_algo(&runtime_arg_to_string(args, 0, ctx.heap()));
    let ikm = latin1_to_bytes(&runtime_arg_to_string(args, 1, ctx.heap()));
    let salt = latin1_to_bytes(&runtime_arg_to_string(args, 2, ctx.heap()));
    let info = latin1_to_bytes(&runtime_arg_to_string(args, 3, ctx.heap()));
    let key_len = number_arg(args, 4).max(0.0) as usize;

    let derived = with_digest(&algo, |block, mac| {
        let digest_len = mac(&[], &[], block).len();
        // An absent salt is a string of zeros as long as the digest.
        let salt = if salt.is_empty() {
            vec![0u8; digest_len]
        } else {
            salt.clone()
        };
        let prk = mac(&salt, &ikm, block);
        hkdf_expand(mac, block, &prk, &info, key_len, digest_len)
    })?;
    crate::string_value(ctx, &bytes_to_latin1(&derived))
}

/// Run `body` with the HMAC of the named digest and that digest's block size.
fn with_digest<R>(
    algo: &str,
    body: impl FnOnce(usize, &dyn Fn(&[u8], &[u8], usize) -> Vec<u8>) -> R,
) -> Result<R, NativeError> {
    match algo {
        "sha1" => Ok(body(64, &|key, data, block| {
            hmac::<sha1::Sha1>(key, data, block)
        })),
        "md5" => Ok(body(64, &|key, data, block| {
            hmac::<md5::Md5>(key, data, block)
        })),
        "sha224" => Ok(body(64, &|key, data, block| {
            hmac::<Sha224>(key, data, block)
        })),
        "sha256" => Ok(body(64, &|key, data, block| {
            hmac::<Sha256>(key, data, block)
        })),
        "sha384" => Ok(body(128, &|key, data, block| {
            hmac::<Sha384>(key, data, block)
        })),
        "sha512" => Ok(body(128, &|key, data, block| {
            hmac::<Sha512>(key, data, block)
        })),
        other => Err(NativeError::Coded {
            kind: otter_vm::ErrorKind::Error,
            code: "ERR_OSSL_EVP_UNSUPPORTED",
            message: format!("Digest method not supported: {other}"),
        }),
    }
}

fn number_arg(args: &[Value], index: usize) -> f64 {
    args.get(index)
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0)
}

/// RFC 2898 §5.2. Each block is `U1 ^ U2 ^ … ^ Uc`, where `U1` covers the salt
/// and the block index and every later `U` is the HMAC of the one before it.
fn pbkdf2(
    mac: &dyn Fn(&[u8], &[u8], usize) -> Vec<u8>,
    block_size: usize,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    key_len: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(key_len);
    let mut block_index: u32 = 1;
    while out.len() < key_len {
        let mut seed = salt.to_vec();
        seed.extend_from_slice(&block_index.to_be_bytes());
        let mut current = mac(password, &seed, block_size);
        let mut block = current.clone();
        for _ in 1..iterations {
            current = mac(password, &current, block_size);
            for (accumulated, byte) in block.iter_mut().zip(current.iter()) {
                *accumulated ^= byte;
            }
        }
        out.extend_from_slice(&block);
        block_index += 1;
    }
    out.truncate(key_len);
    out
}

/// RFC 5869 §2.3 expand.
fn hkdf_expand(
    mac: &dyn Fn(&[u8], &[u8], usize) -> Vec<u8>,
    block_size: usize,
    prk: &[u8],
    info: &[u8],
    key_len: usize,
    digest_len: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(key_len);
    let mut previous: Vec<u8> = Vec::new();
    let mut counter: u8 = 1;
    while out.len() < key_len {
        let mut input = previous.clone();
        input.extend_from_slice(info);
        input.push(counter);
        previous = mac(prk, &input, block_size);
        out.extend_from_slice(&previous);
        counter = counter.wrapping_add(1);
        let _ = digest_len;
    }
    out.truncate(key_len);
    out
}
