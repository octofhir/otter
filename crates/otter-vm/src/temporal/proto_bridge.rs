//! Adapter from `NativeCall::Static` to the per-class
//! [`IntrinsicTable`] entries.
//!
//! Each `Temporal.<Class>` prototype carries the JS-visible methods
//! as own data properties whose [[Value]] is a `NativeFunction`
//! installed via `couch!` `prototype = { method_specs = [...] }`.
//! Those native functions need to land at the same algorithm bodies
//! the dispatcher reaches through
//! [`crate::temporal::lookup_prototype`]. This module exposes a
//! single bridge — [`call_intrinsic`] — that adapts a
//! [`NativeCtx`] call frame into an [`IntrinsicArgs`] frame, invokes
//! the table-resident implementation, and maps
//! [`IntrinsicError`] onto [`NativeError`] honouring the spec error
//! class (`OutOfRange` → `RangeError`, the rest → `TypeError`).
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/>
//! - <https://tc39.es/ecma262/#sec-properties-of-the-temporal-instant-prototype-object>

use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicFn};
use crate::{NativeCtx, NativeError, Value};

/// Generate `NativeCall::Static` wrappers + a `&[MethodSpec]` slice
/// for a `Temporal.<Class>` prototype. Each wrapper bridges
/// [`NativeCtx`] into the per-class `impl_*` body via
/// [`call_intrinsic`].
///
/// Each entry: `"<jsName>" / <length> => <impl_fn> as <native_fn>`.
/// `native_fn` is the wrapper identity; pick a unique name so the
/// generated table compiles next to other methods in the same file.
#[macro_export]
macro_rules! temporal_proto_methods {
    (
        class = $class:literal,
        slice = $slice:ident,
        methods = [ $( $jsname:literal / $len:expr => $impl:ident as $native:ident ),* $(,)? ]
    ) => {
        $(
            fn $native(
                ctx: &mut $crate::NativeCtx<'_>,
                args: &[$crate::Value],
            ) -> ::core::result::Result<$crate::Value, $crate::NativeError> {
                $crate::temporal::proto_bridge::call_intrinsic(
                    concat!("Temporal.", $class, ".prototype.", $jsname),
                    ctx,
                    args,
                    $impl,
                )
            }
        )*
        #[doc = concat!("`Temporal.", $class, ".prototype` `MethodSpec` slice — installed by `couch!` `prototype = { method_specs = [...] }`.")]
        pub static $slice: &[$crate::js_surface::MethodSpec] = &[
            $(
                $crate::js_surface::MethodSpec {
                    name: $jsname,
                    length: $len,
                    attrs: $crate::js_surface::Attr::builtin_function(),
                    call: $crate::native_function::NativeCall::Static($native),
                },
            )*
        ];
    };
}

pub use temporal_proto_methods;

/// Invoke an [`IntrinsicFn`] from a native-function call site.
///
/// The bridge copies the receiver out of the `NativeCtx`,
/// collects the active native roots, builds an `IntrinsicArgs`,
/// and surfaces [`IntrinsicError`] as the spec-correct
/// [`NativeError`].
pub fn call_intrinsic(
    name: &'static str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    impl_fn: IntrinsicFn,
) -> Result<Value, NativeError> {
    let receiver = *ctx.this_value();
    let allocation_roots = ctx.collect_native_roots();
    impl_fn(&mut IntrinsicArgs {
        receiver: &receiver,
        args,
        gc_heap: ctx.heap_mut(),
        allocation_roots: allocation_roots.as_slice(),
    })
    .map_err(|err| intrinsic_to_native(name, err))
}

fn intrinsic_to_native(name: &'static str, err: IntrinsicError) -> NativeError {
    match err {
        IntrinsicError::OutOfRange { index, reason } => NativeError::RangeError {
            name,
            reason: format!("argument {index} out of range: {reason}"),
        },
        IntrinsicError::OutOfMemory { .. } => NativeError::TypeError {
            name,
            reason: "out of memory".to_string(),
        },
        IntrinsicError::BadReceiver { expected } => NativeError::TypeError {
            name,
            reason: format!("invalid receiver: expected {expected}"),
        },
        IntrinsicError::BadArgument { index, reason } => NativeError::TypeError {
            name,
            reason: format!("argument {index} {reason}"),
        },
        IntrinsicError::UnknownMethod { name: method } => NativeError::TypeError {
            name,
            reason: format!("unknown method `{method}`"),
        },
    }
}
