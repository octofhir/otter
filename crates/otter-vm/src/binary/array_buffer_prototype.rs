//! `ArrayBuffer.prototype` / `SharedArrayBuffer.prototype` accessor
//! read path per ECMA-262 §25.1.5.
//!
//! The prototype methods are native functions on the constructor's
//! `couch!` surface; this module only synthesises the `byteLength` /
//! `maxByteLength` / `resizable` / `growable` / `detached` accessor
//! values for `Op::LoadProperty`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-arraybuffer-prototype-object>

use crate::Value;

use super::array_buffer::JsArrayBuffer;
use super::{number_value, smi};

/// §25.1.5 — getter access for `byteLength` / `maxByteLength` /
/// `resizable` / `growable` / `detached`.
#[must_use]
pub fn load_property(buf: JsArrayBuffer, heap: &otter_gc::GcHeap, name: &str) -> Value {
    match name {
        "byteLength" => smi(buf.byte_length(heap) as i32),
        "maxByteLength" => smi(buf.max_byte_length(heap) as i32),
        "resizable" => Value::boolean(buf.is_resizable(heap)),
        // §25.2.4.2 — `growable` for SAB; mirrors `resizable` on
        // ordinary ArrayBuffer.
        "growable" => Value::boolean(buf.is_growable(heap)),
        "detached" => Value::boolean(buf.is_detached(heap)),
        _ => {
            let _ = number_value;
            Value::undefined()
        }
    }
}
