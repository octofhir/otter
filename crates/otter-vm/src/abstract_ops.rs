//! ECMA-262 §7.2 type-check abstract operations as canonical helpers.
//!
//! Single source of truth for the small set of equality / shape
//! predicates the runtime needs everywhere — collection key matching,
//! call dispatch, `Object.is`, `Array.isArray`, `Reflect.construct`,
//! `Reflect.apply`, the upcoming Proxy / Reflect surface (task 81).
//!
//! Implementations are spec-faithful: every helper tracks the same
//! steps the specification text carries so future audits can map
//! line-for-line. Performance work belongs in a later track — these
//! helpers are intentionally simple `match` arms over [`Value`].
//!
//! # Contents
//! - [`same_value`] — §7.2.11 `SameValue`. Backs `Object.is`.
//! - [`same_value_zero`] — §7.2.12 `SameValueZero`. Backs `Map` /
//!   `Set` keying, `Array.prototype.includes`, `String.prototype.includes`.
//! - [`same_value_non_numeric`] — §7.2.13. Shared tail of the two
//!   operations above.
//! - [`is_array`] — §7.2.2 `IsArray`. Today checks the `Value::Array`
//!   tag directly; once Proxy lands the implementation walks the
//!   proxy-target chain.
//! - [`is_callable`] — §7.2.3 `IsCallable`. Recognises every
//!   call-site shape the dispatcher accepts.
//! - [`is_constructor`] — §7.2.4 `IsConstructor`. Subset of
//!   `is_callable`: arrow closures and most native callables answer
//!   `false`.
//!
//! # Invariants
//! - Number equality follows IEEE-754 semantics with the spec's
//!   `+0` / `-0` / `NaN` overrides explicitly applied. `NumberValue`
//!   never normalises `-0.0` into `Smi(0)`, so the helpers can
//!   inspect the sign bit unambiguously.
//! - Cross-kind comparison is always `false` (e.g. `Number` vs
//!   `BigInt`), matching the spec's `Type(x) is not Type(y)` short
//!   circuit.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-samevalue>
//! - <https://tc39.es/ecma262/#sec-samevaluezero>
//! - <https://tc39.es/ecma262/#sec-samevaluenonnumeric>
//! - <https://tc39.es/ecma262/#sec-isarray>
//! - <https://tc39.es/ecma262/#sec-iscallable>
//! - <https://tc39.es/ecma262/#sec-isconstructor>

use crate::Value;
use crate::bigint::ops as bigint_ops;
use crate::execution_context::ExecutionContext;
use crate::number::{self, NumberValue};

/// Preferred primitive type passed to ECMA-262 §7.1.1 `ToPrimitive`.
///
/// - [`Self::Default`] — the abstract operation infers a kind from
///   the receiver. Plain objects act as `Number`; `Date` instances
///   would act as `String` (foundation has no built-in `Date`).
/// - [`Self::Number`] — used by unary `+`, arithmetic, comparisons.
/// - [`Self::String`] — used by `String(x)`, `${x}` interpolation,
///   property keys.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-toprimitive>
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ToPrimitiveHint {
    /// Caller passes `"default"`. The abstract operation defaults
    /// to `"number"` for plain objects.
    Default,
    /// Caller passes `"number"`. Drives `valueOf` first, then
    /// `toString`.
    Number,
    /// Caller passes `"string"`. Drives `toString` first, then
    /// `valueOf`.
    String,
}

impl ToPrimitiveHint {
    /// String token the hint is passed to user code as.
    ///
    /// Per §7.1.1 step 4, `[Symbol.toPrimitive]` receives one of
    /// `"default"`, `"number"`, or `"string"` as its sole argument.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Number => "number",
            Self::String => "string",
        }
    }

    /// Parse the hint string token (`"default"` / `"number"` /
    /// `"string"`) back into the enum.
    ///
    /// # Errors
    /// Returns `None` for any other token.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "default" => Some(Self::Default),
            "number" => Some(Self::Number),
            "string" => Some(Self::String),
            _ => None,
        }
    }
}

/// Return `true` when `value` is already a primitive (`Undefined`,
/// `Null`, `Boolean`, `Number`, `BigInt`, `String`, `Symbol`).
///
/// Mirrors the spec's `Type(value) is not Object` short-circuit
/// in §7.1.1 step 1. The runtime uses this guard at the top of the
/// `Op::ToPrimitive` ladder so already-primitive operands skip the
/// `[Symbol.toPrimitive]` lookup entirely.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-toprimitive>
#[must_use]
pub fn is_primitive(value: &Value) -> bool {
    matches!(
        value,
        Value::Undefined
            | Value::Null
            | Value::Boolean(_)
            | Value::Number(_)
            | Value::BigInt(_)
            | Value::String(_)
            | Value::Symbol(_)
    )
}

/// Return `true` when `x` and `y` are identical under ECMA-262
/// `SameValue` semantics (`Object.is`).
///
/// # Algorithm
/// 1. If `Type(x)` differs from `Type(y)`, return `false`.
/// 2. If both are `Number`:
///    - Both `NaN` → `true`.
///    - `+0` vs `-0` → `false` (sign-bit sensitive).
///    - Otherwise IEEE-754 equality.
/// 3. Otherwise dispatch through [`same_value_non_numeric`].
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-samevalue>
#[must_use]
pub fn same_value(x: &Value, y: &Value, heap: &otter_gc::GcHeap) -> bool {
    match (x, y) {
        (Value::Number(a), Value::Number(b)) => same_value_numeric(*a, *b),
        _ => same_value_non_numeric(x, y, heap),
    }
}

/// Return `true` when `x` and `y` are identical under ECMA-262
/// `SameValueZero` semantics.
///
/// Differs from [`same_value`] only on numeric operands: `+0` and
/// `-0` are treated as equal.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-samevaluezero>
#[must_use]
pub fn same_value_zero(x: &Value, y: &Value, heap: &otter_gc::GcHeap) -> bool {
    match (x, y) {
        (Value::Number(a), Value::Number(b)) => same_value_zero_numeric(*a, *b),
        _ => same_value_non_numeric(x, y, heap),
    }
}

/// Tail of `SameValue` and `SameValueZero` once the numeric
/// short-circuits have been handled.
///
/// Strings compare by code-unit content; symbols, objects, arrays,
/// callables, and other heap-shared values compare by identity; the
/// BigInt arm reads both bodies through `heap` and folds through
/// [`crate::bigint::BigIntValue::numeric_eq`] (spec
/// `SameValueNonNumber` for BigInt-BigInt is numeric equality).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-samevaluenonnumeric>
#[must_use]
pub fn same_value_non_numeric(x: &Value, y: &Value, heap: &otter_gc::GcHeap) -> bool {
    match (x, y) {
        (Value::Number(_), _) | (_, Value::Number(_)) => false,
        // §7.2.13 step 2.b: BigInt-BigInt equality is numeric.
        // `Value::PartialEq`'s BigInt arm uses handle-offset
        // equality and is not spec-correct on its own; route here
        // to `numeric_eq` which reads the bodies through `heap`.
        (Value::BigInt(a), Value::BigInt(b)) => a.numeric_eq(*b, heap),
        // For every other variant `Value::PartialEq` matches the
        // spec's `SameValueNonNumber` reduction (identity for
        // heap-shared shapes, content equality for strings).
        _ => x == y,
    }
}

/// SameValue restricted to two `NumberValue` operands.
///
/// `+0` and `-0` are distinct; `NaN` matches `NaN`.
fn same_value_numeric(a: NumberValue, b: NumberValue) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    let af = a.as_f64();
    let bf = b.as_f64();
    if af == 0.0 && bf == 0.0 {
        // Sign-bit sensitive — `+0` and `-0` differ.
        return af.is_sign_negative() == bf.is_sign_negative();
    }
    af == bf
}

/// SameValueZero restricted to two `NumberValue` operands.
///
/// `+0` and `-0` collapse; `NaN` matches `NaN`.
fn same_value_zero_numeric(a: NumberValue, b: NumberValue) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    a.as_f64() == b.as_f64()
}

/// Return `true` when `value` is an Array exotic object.
///
/// # Algorithm
/// 1. If `value` is `Value::Array`, return `true`.
/// 2. Once Proxy lands (task 81), unwrap proxies whose handler is
///    revoked → `TypeError`; otherwise recurse on the target. Today
///    no Proxy exists, so the helper is a single match arm.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-isarray>
#[must_use]
pub fn is_array(value: &Value) -> bool {
    matches!(value, Value::Array(_))
}

/// Return `true` when `value` carries an internal `[[Call]]` slot.
///
/// Recognises every call-site shape the interpreter dispatches:
/// bytecode functions / closures, bound functions, native
/// callables, and class constructors. Objects with no `[[Call]]`
/// slot — plain objects, arrays, regexes, promises, collections —
/// answer `false`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-iscallable>
#[must_use]
pub fn is_callable(value: &Value) -> bool {
    matches!(
        value,
        Value::Function { .. }
            | Value::Closure(_)
            | Value::BoundFunction(_)
            | Value::NativeFunction(_)
            | Value::ClassConstructor(_)
            // §28.2.1.1 — a Proxy reports `[[Call]]` when its
            // handler defines `apply` (or via target inspection in
            // the wider machinery). Foundation: assume callable; the
            // dispatcher delegates non-callable targets to a proper
            // TypeError on actual call.
            | Value::Proxy(_)
    )
}

/// Return `true` when `value` carries an internal `[[Construct]]`
/// slot — i.e. it is admissible as the callee of `new` or
/// `Reflect.construct`.
///
/// # Algorithm
/// 1. `Value::ClassConstructor` always has `[[Construct]]`.
/// 2. `Value::Function { function_id }` and `Value::Closure { ... }`
///    have `[[Construct]]` iff the underlying bytecode `Function`
///    is **not** an arrow. The check needs the loaded
///    [`ExecutionContext`] for the function-table lookup.
/// 3. `Value::BoundFunction` inherits its target's status.
/// 4. `Value::NativeFunction` carries no `[[Construct]]` today —
///    the foundation lacks a per-callable construct flag. Native
///    constructors are surfaced through dedicated opcodes
///    (`Op::NewArray`, `Op::NewError`, …) rather than `Op::New`.
/// 5. Every other shape returns `false`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-isconstructor>
#[must_use]
pub fn is_constructor(value: &Value, context: &ExecutionContext, heap: &otter_gc::GcHeap) -> bool {
    match value {
        Value::ClassConstructor(_) => true,
        Value::NativeFunction(native) => native.is_constructable(heap),
        Value::Function { function_id }
        | Value::Closure(crate::closure::JsClosure {
            cached_function_id: function_id,
            ..
        }) => !context.function_is_arrow(*function_id),
        Value::BoundFunction(b) => {
            let (target, _, _) = b.parts(heap);
            is_constructor(&target, context, heap)
        }
        // §28.2.4.3 — a non-revoked Proxy reports `[[Construct]]`
        // iff its target does. Revoked proxies surface as
        // non-constructor here; the per-trap revocation guard
        // produces the spec-required TypeError on actual call.
        Value::Proxy(proxy) => {
            !proxy.is_revoked(heap) && is_constructor(&proxy.target(heap), context, heap)
        }
        _ => false,
    }
}

/// Outcome of ECMA-262 §7.2.14 `AbstractRelationalComparison`.
///
/// The spec returns `true` / `false` / `undefined`. The
/// `Undefined` variant signals that any of `<`, `<=`, `>`, `>=`
/// must answer `false` (NaN cascade or `BigInt`-vs-string parse
/// failure).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-abstract-relational-comparison>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RelationalOutcome {
    /// Strict left-of-right ordering.
    LessThan,
    /// `false` arm — operands compared but not strictly less.
    NotLessThan,
    /// Either operand was NaN (Number) or a BigInt-vs-string parse
    /// failed. All four relational operators return `false` here.
    Undefined,
}

/// ECMA-262 §7.2.13 `IsLooselyEqual` (`x == y`) over operands that
/// are already primitives (the caller is expected to have run
/// `Op::ToPrimitive(default)` on each operand).
///
/// # Algorithm
/// 1. Same `Type(x) === Type(y)` → strict equality (handled by
///    [`Value::PartialEq`]).
/// 2. `null == undefined` → `true`.
/// 3. `Number x String` → ToNumber the string, then numeric
///    compare.
/// 4. `BigInt x String` → parse string as BigInt; mismatch → `false`.
/// 5. Boolean → Number coercion, then recurse.
/// 6. `BigInt x Number` → numeric compare with NaN / Infinity
///    handling.
///
/// # Invariants
/// - Caller has already coerced object operands through
///   `ToPrimitive(default)`. Reaching this function with an Object
///   operand returns `false` (no Object × primitive comparison
///   step left to take after the dispatcher's coercion).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-islooselyequal>
#[must_use]
pub fn is_loosely_equal(x: &Value, y: &Value, heap: &otter_gc::GcHeap) -> bool {
    // Step 1: same type → IsStrictlyEqual.
    let same_kind = matches!(
        (x, y),
        (Value::Undefined, Value::Undefined)
            | (Value::Null, Value::Null)
            | (Value::Boolean(_), Value::Boolean(_))
            | (Value::Number(_), Value::Number(_))
            | (Value::BigInt(_), Value::BigInt(_))
            | (Value::String(_), Value::String(_))
            | (Value::Symbol(_), Value::Symbol(_))
    );
    if same_kind {
        return same_value_non_numeric_or_strict_numeric(x, y, heap);
    }

    match (x, y) {
        // Step 2: null == undefined.
        (Value::Null, Value::Undefined) | (Value::Undefined, Value::Null) => true,

        // Steps 4, 5: Number x String — ToNumber the string.
        (Value::Number(n), Value::String(s)) => {
            let parsed = number::to_number_from_string(&s.to_lossy_string());
            number::strict_equals(*n, parsed)
        }
        (Value::String(s), Value::Number(n)) => {
            let parsed = number::to_number_from_string(&s.to_lossy_string());
            number::strict_equals(*n, parsed)
        }

        // Step 6, 8: Boolean → Number, then recurse. We model the
        // recursion as one step rather than re-entering this
        // function (avoids needing a fresh `Value`).
        (Value::Boolean(b), other) | (other, Value::Boolean(b)) => {
            let coerced = Value::Number(NumberValue::from_i32(if *b { 1 } else { 0 }));
            is_loosely_equal(&coerced, other, heap)
        }

        // Steps 12: BigInt x String. §7.1.14 StringToBigInt:
        // whitespace-only / empty strings are valid
        // StringIntegerLiterals representing `0n`. Strings that fail
        // the grammar surface as `undefined`, which §7.2.13 step 8
        // collapses to `false`.
        (Value::BigInt(big), Value::String(s)) | (Value::String(s), Value::BigInt(big)) => {
            match string_to_big_int(&s.to_lossy_string()) {
                Some(parsed) => big.with_inner(heap, |b| b == &parsed),
                None => false,
            }
        }

        // Steps 13, 14: BigInt x Number.
        (Value::BigInt(big), Value::Number(num)) | (Value::Number(num), Value::BigInt(big)) => {
            big.with_inner(heap, |b| bigint_eq_number(b, *num))
        }

        _ => false,
    }
}

/// Spec `IsStrictlyEqual` for two operands of the same type as
/// determined by [`is_loosely_equal`] step 1.
///
/// Numbers use `number::strict_equals` so `NaN !== NaN` and
/// `+0 === -0`. BigInt-BigInt routes through `numeric_eq(heap)`
/// (handle-offset equality is not spec-correct). Every other
/// variant defers to `Value::PartialEq`.
fn same_value_non_numeric_or_strict_numeric(x: &Value, y: &Value, heap: &otter_gc::GcHeap) -> bool {
    if let (Value::Number(a), Value::Number(b)) = (x, y) {
        return number::strict_equals(*a, *b);
    }
    if let (Value::BigInt(a), Value::BigInt(b)) = (x, y) {
        return a.numeric_eq(*b, heap);
    }
    x == y
}

/// §7.1.14 `StringToBigInt(str)`. Whitespace-trims `str` and
/// accepts:
///
/// - empty / whitespace-only strings → `0n`;
/// - decimal integer literals (with optional `+` / `-` sign);
/// - non-decimal integer literals (`0x…`, `0o…`, `0b…`).
///
/// Returns `None` when `str` does not match the grammar — callers
/// surface that as the spec's `undefined` outcome.
pub fn string_to_big_int(text: &str) -> Option<num_bigint::BigInt> {
    let s = text.trim();
    if s.is_empty() {
        return Some(num_bigint::BigInt::from(0));
    }
    let (sign_negative, body) = match s.as_bytes().first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    if body.is_empty() {
        return None;
    }
    let parsed = if let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        // §12.9.3.1 NonDecimalIntegerLiteral — no sign allowed in
        // the non-decimal form per the spec grammar; reject when
        // we saw an explicit sign above.
        if sign_negative {
            return None;
        }
        num_bigint::BigInt::parse_bytes(rest.as_bytes(), 16)?
    } else if let Some(rest) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
        if sign_negative {
            return None;
        }
        num_bigint::BigInt::parse_bytes(rest.as_bytes(), 8)?
    } else if let Some(rest) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        if sign_negative {
            return None;
        }
        num_bigint::BigInt::parse_bytes(rest.as_bytes(), 2)?
    } else {
        num_bigint::BigInt::parse_bytes(body.as_bytes(), 10)?
    };
    Some(if sign_negative { -parsed } else { parsed })
}

/// `bigint == number` — only true when `number` is finite and
/// integer-valued and matches the bigint's exact decimal form.
fn bigint_eq_number(big: &num_bigint::BigInt, num: NumberValue) -> bool {
    let f = num.as_f64();
    if !f.is_finite() {
        return false;
    }
    if f.fract() != 0.0 {
        return false;
    }
    matches!(
        bigint_ops::compare_to_f64(big, f),
        Some(std::cmp::Ordering::Equal)
    )
}

/// ECMA-262 §7.2.14 `AbstractRelationalComparison` over operands
/// that the caller has already coerced through
/// `Op::ToPrimitive(number)`.
///
/// # Algorithm
/// 1. Both operands `String` → lexicographic code-unit ordering.
/// 2. `BigInt x String` (and vice versa) → parse the string. On
///    parse failure, return `Undefined`.
/// 3. Otherwise both go through `ToNumeric`. Mixed `Number x BigInt`
///    compares numerically with NaN / `Infinity` handling.
///
/// `left_first` is the spec's evaluation-order flag — irrelevant
/// here because operands are already primitives.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-abstract-relational-comparison>
#[must_use]
pub fn abstract_relational_comparison(
    x: &Value,
    y: &Value,
    heap: &otter_gc::GcHeap,
) -> RelationalOutcome {
    // Step 1: both String → lexicographic.
    if let (Value::String(a), Value::String(b)) = (x, y) {
        return match a.compare_lex(b) {
            std::cmp::Ordering::Less => RelationalOutcome::LessThan,
            _ => RelationalOutcome::NotLessThan,
        };
    }
    // Step 2 / 3: BigInt x String.
    if let (Value::BigInt(big), Value::String(s)) = (x, y) {
        return match string_to_big_int(&s.to_lossy_string()) {
            Some(parsed) => match big.with_inner(heap, |b| bigint_ops::compare(b, &parsed)) {
                std::cmp::Ordering::Less => RelationalOutcome::LessThan,
                _ => RelationalOutcome::NotLessThan,
            },
            None => RelationalOutcome::Undefined,
        };
    }
    if let (Value::String(s), Value::BigInt(big)) = (x, y) {
        return match string_to_big_int(&s.to_lossy_string()) {
            Some(parsed) => match big.with_inner(heap, |b| bigint_ops::compare(&parsed, b)) {
                std::cmp::Ordering::Less => RelationalOutcome::LessThan,
                _ => RelationalOutcome::NotLessThan,
            },
            None => RelationalOutcome::Undefined,
        };
    }

    // Numeric coercion path.
    let lnum = to_numeric_for_compare(x, heap);
    let rnum = to_numeric_for_compare(y, heap);
    match (lnum, rnum) {
        (Some(NumericKind::Num(a)), Some(NumericKind::Num(b))) => match number::compare(a, b) {
            number::NumericOrdering::Less => RelationalOutcome::LessThan,
            number::NumericOrdering::Equal | number::NumericOrdering::Greater => {
                RelationalOutcome::NotLessThan
            }
            number::NumericOrdering::Unordered => RelationalOutcome::Undefined,
        },
        (Some(NumericKind::Big(a)), Some(NumericKind::Big(b))) => {
            match bigint_ops::compare(&a, &b) {
                std::cmp::Ordering::Less => RelationalOutcome::LessThan,
                _ => RelationalOutcome::NotLessThan,
            }
        }
        (Some(NumericKind::Big(a)), Some(NumericKind::Num(b))) => {
            match bigint_ops::compare_to_f64(&a, b.as_f64()) {
                Some(std::cmp::Ordering::Less) => RelationalOutcome::LessThan,
                Some(_) => RelationalOutcome::NotLessThan,
                None => RelationalOutcome::Undefined,
            }
        }
        (Some(NumericKind::Num(a)), Some(NumericKind::Big(b))) => {
            match bigint_ops::compare_to_f64(&b, a.as_f64()) {
                Some(std::cmp::Ordering::Less) => RelationalOutcome::NotLessThan,
                Some(std::cmp::Ordering::Equal) => RelationalOutcome::NotLessThan,
                Some(std::cmp::Ordering::Greater) => RelationalOutcome::LessThan,
                None => RelationalOutcome::Undefined,
            }
        }
        _ => RelationalOutcome::Undefined,
    }
}

/// Numeric union used by
/// [`abstract_relational_comparison`] and the binary-op runtime
/// in `lib.rs`. The two variants reflect §7.1.4 ToNumeric's
/// product type — Number for everything except BigInt.
pub enum NumericKind {
    /// Operand reduced to a Number (covers `Number`, `String`,
    /// `Boolean`, `null`, `undefined`).
    Num(NumberValue),
    /// Operand reduced to an owned [`num_bigint::BigInt`]. The body
    /// is cloned out of the GC heap once so downstream comparisons
    /// / arithmetic do not have to re-borrow the heap.
    Big(num_bigint::BigInt),
}

/// §7.1.4 ToNumeric over an already-primitive Value. Mirrors
/// the spec's per-type table: Number passes through, BigInt
/// passes through, String parses via `to_number_from_string`,
/// Boolean folds to 0/1, null → 0, undefined → NaN. Symbols
/// and any non-primitive variant return `None` so the caller
/// can surface a TypeError.
///
/// Spec: <https://tc39.es/ecma262/#sec-tonumeric>
pub fn to_numeric_kind(value: &Value, heap: &otter_gc::GcHeap) -> Option<NumericKind> {
    match value {
        Value::Number(n) => Some(NumericKind::Num(*n)),
        Value::BigInt(b) => Some(NumericKind::Big(b.clone_inner(heap))),
        Value::String(s) => Some(NumericKind::Num(number::to_number_from_string(
            &s.to_lossy_string(),
        ))),
        Value::Boolean(true) => Some(NumericKind::Num(NumberValue::from_i32(1))),
        Value::Boolean(false) => Some(NumericKind::Num(NumberValue::from_i32(0))),
        Value::Null => Some(NumericKind::Num(NumberValue::from_i32(0))),
        Value::Undefined => Some(NumericKind::Num(NumberValue::Double(f64::NAN))),
        Value::Symbol(_) => None,
        _ => None,
    }
}

fn to_numeric_for_compare(value: &Value, heap: &otter_gc::GcHeap) -> Option<NumericKind> {
    to_numeric_kind(value, heap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;
    use crate::string::JsString;

    fn n(v: f64) -> Value {
        Value::Number(NumberValue::Double(v))
    }

    fn s(v: &str, heap: &otter_gc::GcHeap) -> Value {
        Value::String(JsString::from_str(v, heap).expect("foundation heap fits the literal"))
    }

    fn fresh_heap() -> otter_gc::GcHeap {
        otter_gc::GcHeap::new().expect("gc heap")
    }

    #[test]
    fn same_value_distinguishes_signed_zeros() {
        let heap = fresh_heap();
        assert!(!same_value(&n(0.0), &n(-0.0), &heap));
        assert!(same_value_zero(&n(0.0), &n(-0.0), &heap));
    }

    #[test]
    fn nan_equal_under_both_helpers() {
        let heap = fresh_heap();
        let nan = n(f64::NAN);
        assert!(same_value(&nan, &nan, &heap));
        assert!(same_value_zero(&nan, &nan, &heap));
    }

    #[test]
    fn cross_kind_rejected() {
        let heap = fresh_heap();
        assert!(!same_value(&n(1.0), &Value::Boolean(true), &heap));
        assert!(!same_value_zero(&n(1.0), &Value::Boolean(true), &heap));
        assert!(!same_value(&Value::Null, &Value::Undefined, &heap));
    }

    #[test]
    fn strings_compare_by_content() {
        let heap = fresh_heap();
        assert!(same_value(&s("hi", &heap), &s("hi", &heap), &heap));
        assert!(!same_value(&s("hi", &heap), &s("bye", &heap), &heap));
    }

    #[test]
    fn primitives_match() {
        let heap = fresh_heap();
        assert!(same_value(&Value::Undefined, &Value::Undefined, &heap));
        assert!(same_value(&Value::Null, &Value::Null, &heap));
        assert!(same_value(
            &Value::Boolean(true),
            &Value::Boolean(true),
            &heap
        ));
        assert!(!same_value(
            &Value::Boolean(true),
            &Value::Boolean(false),
            &heap
        ));
    }

    #[test]
    fn is_array_recognises_array_only() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        assert!(is_array(&Value::Array(
            crate::array::alloc_array_old_for_fixture(&mut heap).unwrap()
        )));
        assert!(!is_array(&Value::Object(
            crate::object::alloc_object_old_for_fixture(&mut heap).unwrap()
        )));
        assert!(!is_array(&Value::Undefined));
    }

    #[test]
    fn is_callable_recognises_call_shapes() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        assert!(is_callable(&Value::Function { function_id: 0 }));
        assert!(!is_callable(&Value::Object(
            crate::object::alloc_object_old_for_fixture(&mut heap).unwrap()
        )));
        assert!(!is_callable(&Value::Undefined));
    }
}
