//! Web Crypto namespace (`crypto`), declared through `#[js_namespace]`.
//!
//! The compute primitives are native members of the namespace itself:
//! `randomUUID` is fully native, while `getRandomValues` and
//! `subtle.digest` split WebIDL validation (exact `DOMException`
//! names — `TypeMismatchError`, `QuotaExceededError`,
//! `NotSupportedError`) into the attached `crypto.ns.js` glue and
//! keep the CSPRNG fill / hashing native. The glue consumes the
//! private `__nativeRandomFill` / `__nativeDigest` members and then
//! deletes them, so nothing hidden leaks onto the global object —
//! the old `__otterCrypto*` global registrations are gone.
//!
//! # Contents
//! - [`WebCrypto`] — the namespace declaration.
//! - `crypto.ns.js` — validation glue + `SubtleCrypto` shape.
//!
//! # Invariants
//! - All randomness comes from the operating-system CSPRNG
//!   (`getrandom`); there is no userspace PRNG fallback — OS failure
//!   surfaces as a catchable error.
//! - Native members re-check argument shapes: the JS glue owns spec
//!   conformance (exact DOMException names), the native layer never
//!   trusts its inputs.
//!
//! # See also
//! - <https://w3c.github.io/webcrypto/>

use std::fmt::Write as _;

use otter_macros::js_namespace;
use otter_runtime::marshal::{ArrayBuffer, BufferSource, JsError};
use otter_runtime::{
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError, RuntimeValue as Value,
    runtime_type_error,
};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

/// Marker type for the `crypto` namespace declaration.
pub struct WebCrypto;

/// Fill `dest` from the OS CSPRNG. There is deliberately no PRNG
/// fallback: if the OS entropy source fails, the caller sees a
/// catchable error rather than weak randomness.
fn os_random(dest: &mut [u8]) -> Result<(), JsError> {
    getrandom::fill(dest).map_err(|err| JsError::Type(format!("OS randomness unavailable: {err}")))
}

#[js_namespace(name = "crypto", feature = WEB, tag = "Crypto", js = "crypto.ns.js")]
impl WebCrypto {
    /// RFC 9562 version-4 UUID from 16 CSPRNG bytes: version nibble
    /// `4`, variant bits `10`, lowercase hex.
    #[method(name = "randomUUID")]
    fn random_uuid() -> Result<String, JsError> {
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
        Ok(out)
    }

    /// CSPRNG fill of a typed array's live byte range, returning the
    /// same view. Spec validation (integer element type, 65536-byte
    /// quota, exact DOMException names) lives in `crypto.ns.js`; this
    /// member is consumed and deleted by the glue. `raw` because it
    /// mutates the argument's backing store in place.
    #[method(name = "__nativeRandomFill", length = 1, raw)]
    fn native_random_fill(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
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
        os_random(&mut random).map_err(|err| err.into_native("crypto.getRandomValues"))?;
        view.buffer(ctx.heap())
            .with_bytes_mut(ctx.heap_mut(), |bytes| {
                // A detached buffer yields an empty byte vector; the
                // range check makes that (and any stale view metadata)
                // a no-op write.
                if let Some(target) = bytes.get_mut(offset..offset + length) {
                    target.copy_from_slice(&random);
                }
            });
        Ok(value)
    }

    /// Hash a BufferSource with the named algorithm and return a
    /// fresh `ArrayBuffer`. Algorithm-name normalization and the
    /// `NotSupportedError` DOMException live in `crypto.ns.js`; this
    /// accepts only the exact normalized names. Consumed and deleted
    /// by the glue.
    #[method(name = "__nativeDigest")]
    async fn native_digest(algorithm: String, data: BufferSource) -> Result<ArrayBuffer, JsError> {
        let input = data.into_bytes();
        let hash: Vec<u8> = match algorithm.as_str() {
            "SHA-1" => Sha1::digest(&input).to_vec(),
            "SHA-256" => Sha256::digest(&input).to_vec(),
            "SHA-384" => Sha384::digest(&input).to_vec(),
            "SHA-512" => Sha512::digest(&input).to_vec(),
            other => {
                return Err(JsError::Type(format!(
                    "unsupported digest algorithm '{other}'"
                )));
            }
        };
        Ok(ArrayBuffer(hash))
    }
}
