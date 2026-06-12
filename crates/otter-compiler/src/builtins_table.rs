//! Small lookup tables for builtins lowered directly by the compiler.
//!
//! # Contents
//! - typed-array and error-class predicates
//! - static constant tables
//! - fast-path eligibility checks
//!
//! # Invariants
//! - Tables are pure and side-effect free.
//!
//! # See also
//! - `builtins_call` for emitted builtin call lowering

/// Find the deepest [`ModuleState`] frame that declares an
/// imported alias matching `name`. Returns the binding info
/// Whether `name` is one of the eleven canonical TypedArray
/// constructor names per ECMA-262 Table 71. Used by the compiler to
/// inline `<T>.BYTES_PER_ELEMENT` reads.
pub(crate) fn is_typed_array_name(name: &str) -> bool {
    matches!(
        name,
        "Int8Array"
            | "Uint8Array"
            | "Uint8ClampedArray"
            | "Int16Array"
            | "Uint16Array"
            | "Int32Array"
            | "Uint32Array"
            | "Float32Array"
            | "Float64Array"
            | "BigInt64Array"
            | "BigUint64Array"
    )
}

/// `true` when `name` is one of the seven canonical native error
/// classes (`Error`, `TypeError`, `RangeError`, `SyntaxError`,
/// `ReferenceError`, `URIError`, `EvalError`).
///
/// Used by [`compile_expr`] (bare-identifier read) and
/// [`compile_method_call`] / new-expression lowering. Local
/// bindings of the same name take precedence — callers must
/// confirm `lookup_binding` and `find_module_import_binding` both
/// returned `None` before consulting this helper.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
pub(crate) fn is_builtin_error_class_name(name: &str) -> bool {
    matches!(
        name,
        "Error"
            | "TypeError"
            | "RangeError"
            | "SyntaxError"
            | "ReferenceError"
            | "URIError"
            | "EvalError"
            | "AggregateError"
    )
}

/// §21.1.1 Number static constants. Returns the IEEE-754 value the
/// compiler inlines via `Op::LoadNumber` when the user reads
/// `Number.<CONST>` outside any local shadow.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-value-properties-of-the-number-constructor>
pub(crate) fn number_static_constant(name: &str) -> Option<f64> {
    Some(match name {
        // §21.1.1.6
        "MAX_SAFE_INTEGER" => 9_007_199_254_740_991.0,
        // §21.1.1.10
        "MIN_SAFE_INTEGER" => -9_007_199_254_740_991.0,
        // §21.1.1.4
        "MAX_VALUE" => f64::MAX,
        // §21.1.1.7 — smallest positive subnormal.
        "MIN_VALUE" => 5e-324,
        // §21.1.1.1 — 2^-52.
        "EPSILON" => f64::EPSILON,
        // §21.1.1.11 / §21.1.1.9
        "POSITIVE_INFINITY" => f64::INFINITY,
        "NEGATIVE_INFINITY" => f64::NEG_INFINITY,
        // §21.1.1.8
        "NaN" => f64::NAN,
        _ => return None,
    })
}

/// §21.3.1 Math value properties. Returns the names the compiler
/// may route through `Op::MathLoad`; method properties must remain
/// ordinary loads so `Math.abs.length` and extracted calls observe
/// the real namespace installed by bootstrap.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-value-properties-of-the-math-object>
pub(crate) fn math_static_constant(name: &str) -> Option<()> {
    match name {
        "E" | "LN10" | "LN2" | "LOG10E" | "LOG2E" | "PI" | "SQRT1_2" | "SQRT2" => Some(()),
        _ => None,
    }
}

/// `true` when the `NewBuiltinError` fast path can lower this
/// call without losing semantics — i.e. when the call only
/// supplies the operand shapes the opcode encodes.
///
/// `new Error(message, options)` / `new TypeError(message,
/// options)` etc. fall through to the real runtime constructor:
/// `InstallErrorCause` must perform `HasProperty`/`Get`, preserve
/// abrupt completions, install a non-enumerable property, and run
/// after observable message `ToString`.
pub(crate) fn builtin_error_construct_fast_path_applies(
    kind: &str,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
) -> bool {
    if kind == "AggregateError" {
        return false;
    }
    let cause_arg_index = if kind == "AggregateError" { 2 } else { 1 };
    arguments.len() <= cause_arg_index
}
