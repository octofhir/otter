//! The standard-library builtins an asm.js module may import, by identity.
//!
//! asm.js linking (V8 `AreStdlibMembersValid`) accepts a `stdlib` member only
//! when it is the engine's own builtin, never a lookalike: a `Math` function
//! or typed-array constructor is recognized by the static native entry every
//! realm's copy shares, so a replaced global cannot pass.
//!
//! # Contents
//! - [`StdlibFunction`] — the importable functions.
//! - [`is_builtin`] — the identity test.
//! - [`math_f64`] — the members' arithmetic, shared with the builtins.
//!
//! # Invariants
//! - Identity is the interned external reference of the builtin's static
//!   entry; a dynamic or user callable never matches.
//!
//! # See also
//! - <http://asmjs.org/spec/latest/#standard-library>
//! - [`crate::math::apply_f64`] — the arithmetic every `Math` call shares.

use crate::Value;

/// A function member of the asm.js standard library.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StdlibFunction {
    /// `Math.acos`.
    Acos,
    /// `Math.asin`.
    Asin,
    /// `Math.atan`.
    Atan,
    /// `Math.cos`.
    Cos,
    /// `Math.sin`.
    Sin,
    /// `Math.tan`.
    Tan,
    /// `Math.exp`.
    Exp,
    /// `Math.log`.
    Log,
    /// `Math.ceil`.
    Ceil,
    /// `Math.floor`.
    Floor,
    /// `Math.sqrt`.
    Sqrt,
    /// `Math.abs`.
    Abs,
    /// `Math.min`.
    Min,
    /// `Math.max`.
    Max,
    /// `Math.atan2`.
    Atan2,
    /// `Math.pow`.
    Pow,
    /// `Math.imul`.
    Imul,
    /// `Math.clz32`.
    Clz32,
    /// `Math.fround`.
    Fround,
    /// `Int8Array`.
    Int8Array,
    /// `Uint8Array`.
    Uint8Array,
    /// `Int16Array`.
    Int16Array,
    /// `Uint16Array`.
    Uint16Array,
    /// `Int32Array`.
    Int32Array,
    /// `Uint32Array`.
    Uint32Array,
    /// `Float32Array`.
    Float32Array,
    /// `Float64Array`.
    Float64Array,
}

impl StdlibFunction {
    /// The static native entry of the builtin.
    fn entry(self) -> usize {
        use crate::bootstrap_typed_array as ta;
        use crate::math;
        let entry: crate::NativeFastFn = match self {
            Self::Acos => math::native_acos,
            Self::Asin => math::native_asin,
            Self::Atan => math::native_atan,
            Self::Cos => math::native_cos,
            Self::Sin => math::native_sin,
            Self::Tan => math::native_tan,
            Self::Exp => math::native_exp,
            Self::Log => math::native_log,
            Self::Ceil => math::native_ceil,
            Self::Floor => math::native_floor,
            Self::Sqrt => math::native_sqrt,
            Self::Abs => math::native_abs,
            Self::Min => math::native_min,
            Self::Max => math::native_max,
            Self::Atan2 => math::native_atan2,
            Self::Pow => math::native_pow,
            Self::Imul => math::native_imul,
            Self::Clz32 => math::native_clz32,
            Self::Fround => math::native_fround,
            Self::Int8Array => ta::ctor_int8,
            Self::Uint8Array => ta::ctor_uint8,
            Self::Int16Array => ta::ctor_int16,
            Self::Uint16Array => ta::ctor_uint16,
            Self::Int32Array => ta::ctor_int32,
            Self::Uint32Array => ta::ctor_uint32,
            Self::Float32Array => ta::ctor_float32,
            Self::Float64Array => ta::ctor_float64,
        };
        entry as usize
    }
}

/// `true` when `value` is the engine's own `function` builtin, from any realm.
#[must_use]
pub fn is_builtin(value: Value, function: StdlibFunction, heap: &otter_gc::GcHeap) -> bool {
    let Some(native) = value.as_native_function() else {
        return false;
    };
    let Some(expected) = heap.external_refs().lookup(function.entry()) else {
        return false;
    };
    native.native_ref(heap) == Some(expected)
}

/// The arithmetic of a `Math` member over doubles, shared with the `Math`
/// builtins so a translated module computes bit-identical results. `None`
/// for a typed-array constructor.
#[must_use]
pub fn math_f64(function: StdlibFunction, args: &[f64]) -> Option<f64> {
    use otter_bytecode::method_id::MathMethod as M;
    let method = match function {
        StdlibFunction::Acos => M::Acos,
        StdlibFunction::Asin => M::Asin,
        StdlibFunction::Atan => M::Atan,
        StdlibFunction::Cos => M::Cos,
        StdlibFunction::Sin => M::Sin,
        StdlibFunction::Tan => M::Tan,
        StdlibFunction::Exp => M::Exp,
        StdlibFunction::Log => M::Log,
        StdlibFunction::Ceil => M::Ceil,
        StdlibFunction::Floor => M::Floor,
        StdlibFunction::Sqrt => M::Sqrt,
        StdlibFunction::Abs => M::Abs,
        StdlibFunction::Min => M::Min,
        StdlibFunction::Max => M::Max,
        StdlibFunction::Atan2 => M::Atan2,
        StdlibFunction::Pow => M::Pow,
        StdlibFunction::Imul => M::Imul,
        StdlibFunction::Clz32 => M::Clz32,
        StdlibFunction::Fround => M::Fround,
        _ => return None,
    };
    Some(crate::math::apply_f64(method, args))
}
