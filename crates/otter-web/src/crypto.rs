//! Native backing for the Web Crypto global (`crypto`).
//!
//! The JS-facing surface (`crypto.getRandomValues`, `crypto.randomUUID`,
//! `crypto.subtle.digest`) is defined in `web_bootstrap.js`, which performs
//! the WebIDL argument validation (`TypeMismatchError` / `QuotaExceededError`
//! / `NotSupportedError` DOMExceptions) and delegates to the hidden
//! `__otterCrypto*` native globals registered here.
//!
//! # Contents
//! - `install` - registers the hidden `__otterCrypto*` native globals.
//! - `random_fill` - CSPRNG fill of a typed array's byte range.
//! - `random_uuid` - RFC 9562 version-4 UUID string.
//! - `digest` - SHA-1 / SHA-2 digests returned as an `ArrayBuffer`.
//!
//! # Invariants
//! - All randomness comes from the operating-system CSPRNG (`getrandom`);
//!   there is no userspace PRNG fallback — OS failure surfaces as an error.
//! - Native functions re-check argument shapes: the JS shim owns spec
//!   conformance (exact DOMException names), the native layer never trusts
//!   its inputs.
//!
//! # See also
//! - [`crate::globals`] - installs these natives and the lazy JS shim.

use std::fmt::Write as _;

use otter_runtime::{
    OtterError, Runtime, RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeValue as Value, runtime_arg_to_string, runtime_string_value, runtime_type_error,
};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

/// Register the hidden native globals backing the `crypto` JS shim.
pub(crate) fn install(runtime: &mut Runtime) -> Result<(), OtterError> {
    runtime.install_native_global("__otterCryptoRandomFill", 1, random_fill)?;
    runtime.install_native_global("__otterCryptoRandomUUID", 0, random_uuid)?;
    runtime.install_native_global("__otterCryptoDigest", 2, digest)?;
    Ok(())
}

/// Fill `dest` from the OS CSPRNG. There is deliberately no PRNG fallback:
/// if the OS entropy source fails, the caller sees a catchable error rather
/// than weak randomness.
fn os_random(dest: &mut [u8]) -> Result<(), NativeError> {
    getrandom::fill(dest)
        .map_err(|err| runtime_type_error("crypto", format!("OS randomness unavailable: {err}")))
}

/// `__otterCryptoRandomFill(view)` — overwrite the view's byte range with
/// CSPRNG output and return the same view. Spec-side validation (integer
/// element type, 65536-byte quota) happens in the JS shim.
fn random_fill(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    let Some(view) = value.as_typed_array(ctx.heap()) else {
        return Err(runtime_type_error(
            "getRandomValues",
            "argument must be an integer TypedArray",
        ));
    };
    let offset = view.byte_offset(ctx.heap());
    let length = view.byte_length(ctx.heap());
    if length == 0 {
        return Ok(value);
    }
    let mut random = vec![0u8; length];
    os_random(&mut random)?;
    view.buffer(ctx.heap())
        .with_bytes_mut(ctx.heap_mut(), |bytes| {
            // A detached buffer yields an empty byte vector; the range check
            // makes that (and any stale view metadata) a no-op write.
            if let Some(target) = bytes.get_mut(offset..offset + length) {
                target.copy_from_slice(&random);
            }
        });
    Ok(value)
}

/// `__otterCryptoRandomUUID()` — RFC 9562 version-4 UUID from 16 CSPRNG
/// bytes: version nibble `4`, variant bits `10`, lowercase hex.
fn random_uuid(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let mut bytes = [0u8; 16];
    os_random(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{byte:02x}");
    }
    runtime_string_value(ctx, &out)
}

/// `__otterCryptoDigest(algorithm, data)` — hash a BufferSource with the
/// named algorithm and return a fresh `ArrayBuffer` holding the digest.
/// Algorithm-name normalization and the `NotSupportedError` DOMException
/// live in the JS shim; this accepts only the exact normalized names.
fn digest(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let algorithm = runtime_arg_to_string(args, 0, ctx.heap());
    let data = args.get(1).copied().unwrap_or_else(Value::undefined);
    let input: Vec<u8> = if let Some(view) = data.as_typed_array(ctx.heap()) {
        let offset = view.byte_offset(ctx.heap());
        let length = view.byte_length(ctx.heap());
        view.buffer(ctx.heap())
            .with_bytes(ctx.heap(), |bytes| {
                bytes.get(offset..offset + length).map(<[u8]>::to_vec)
            })
            .unwrap_or_default()
    } else if let Some(buffer) = data.as_array_buffer() {
        buffer.with_bytes(ctx.heap(), |bytes| bytes.to_vec())
    } else {
        return Err(runtime_type_error(
            "SubtleCrypto.digest",
            "data must be an ArrayBuffer or ArrayBufferView",
        ));
    };
    let hash: Vec<u8> = match algorithm.as_str() {
        "SHA-1" => Sha1::digest(&input).to_vec(),
        "SHA-256" => Sha256::digest(&input).to_vec(),
        "SHA-384" => Sha384::digest(&input).to_vec(),
        "SHA-512" => Sha512::digest(&input).to_vec(),
        other => {
            return Err(runtime_type_error(
                "SubtleCrypto.digest",
                format!("unsupported digest algorithm '{other}'"),
            ));
        }
    };
    let buffer = ctx
        .array_buffer_from_bytes_rooted(hash, &[], &[])
        .map_err(|err| runtime_type_error("SubtleCrypto.digest", err.to_string()))?;
    Ok(Value::array_buffer(buffer))
}
