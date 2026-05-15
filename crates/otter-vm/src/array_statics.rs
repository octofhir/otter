//! `Array.<static>` dispatchers and JS-visible static method specs.
//!
//! Each Array static surface has its own typed entry point —
//! The active `Array(...)`, `Array.from`, and `Array.of` opcode
//! paths live in [`crate::array_ops`] because they can expose the VM
//! frame stack to root-aware allocation. This module only owns the
//! JS-visible static method specs installed on the constructor.
//!
//! # Contents
//! - [`ARRAY_STATIC_METHODS`] — methods installed on the `Array`
//!   constructor during bootstrap.
//!
//! # Invariants
//! - Constructor static methods that allocate arrays stay in
//!   [`crate::array_ops`] so they use active-frame roots.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-array-constructor>
//! - <https://tc39.es/ecma262/#sec-array>
//! - <https://tc39.es/ecma262/#sec-array.from>
//! - <https://tc39.es/ecma262/#sec-array.of>

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::{NativeCtx, NativeError, Value};

/// Static methods installed on the `Array` constructor.
pub static ARRAY_STATIC_METHODS: &[MethodSpec] = &[MethodSpec {
    name: "isArray",
    length: 1,
    attrs: Attr::builtin_function(),
    call: NativeCall::Static(native_is_array),
}];

fn native_is_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::Boolean(matches!(
        args.first(),
        Some(Value::Array(_))
    )))
}
