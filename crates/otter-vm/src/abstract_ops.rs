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
use crate::bigint::{BigIntValue, ops as bigint_ops};
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
pub fn same_value(x: &Value, y: &Value) -> bool {
    match (x, y) {
        (Value::Number(a), Value::Number(b)) => same_value_numeric(*a, *b),
        _ => same_value_non_numeric(x, y),
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
pub fn same_value_zero(x: &Value, y: &Value) -> bool {
    match (x, y) {
        (Value::Number(a), Value::Number(b)) => same_value_zero_numeric(*a, *b),
        _ => same_value_non_numeric(x, y),
    }
}

/// Tail of `SameValue` and `SameValueZero` once the numeric
/// short-circuits have been handled.
///
/// Strings compare by code-unit content; symbols, objects, arrays,
/// callables, and other heap-shared values compare by identity
/// (matching the existing `Value::PartialEq` definitions).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-samevaluenonnumeric>
#[must_use]
pub fn same_value_non_numeric(x: &Value, y: &Value) -> bool {
    match (x, y) {
        (Value::Number(_), _) | (_, Value::Number(_)) => false,
        // `Value::PartialEq` already implements identity for every
        // heap shape and content equality for strings; cross-kind
        // pairs short-circuit there. The spec text for
        // `SameValueNonNumber` reduces to that exact behaviour for
        // Undefined, Null, Boolean, BigInt, String, Symbol, and
        // every Object-typed variant.
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
            | Value::Closure { .. }
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
        Value::Function { function_id } | Value::Closure { function_id, .. } => {
            !context.function_is_arrow(*function_id)
        }
        Value::BoundFunction(b) => {
            let (target, _, _) = b.parts(heap);
            is_constructor(&target, context, heap)
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
pub fn is_loosely_equal(x: &Value, y: &Value) -> bool {
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
        return same_value_non_numeric_or_strict_numeric(x, y);
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
            is_loosely_equal(&coerced, other)
        }

        // Steps 12: BigInt x String.
        (Value::BigInt(big), Value::String(s)) | (Value::String(s), Value::BigInt(big)) => {
            match BigIntValue::from_decimal(s.to_lossy_string().trim()) {
                Some(parsed) => big == &parsed,
                None => false,
            }
        }

        // Steps 13, 14: BigInt x Number.
        (Value::BigInt(big), Value::Number(num)) | (Value::Number(num), Value::BigInt(big)) => {
            bigint_eq_number(big, *num)
        }

        _ => false,
    }
}

/// Spec `IsStrictlyEqual` for two operands of the same type as
/// determined by [`is_loosely_equal`] step 1.
///
/// Numbers use `number::strict_equals` so `NaN !== NaN` and
/// `+0 === -0`. Every other variant defers to `Value::PartialEq`.
fn same_value_non_numeric_or_strict_numeric(x: &Value, y: &Value) -> bool {
    if let (Value::Number(a), Value::Number(b)) = (x, y) {
        return number::strict_equals(*a, *b);
    }
    x == y
}

/// `bigint == number` — only true when `number` is finite and
/// integer-valued and matches the bigint's exact decimal form.
fn bigint_eq_number(big: &BigIntValue, num: NumberValue) -> bool {
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
pub fn abstract_relational_comparison(x: &Value, y: &Value) -> RelationalOutcome {
    // Step 1: both String → lexicographic.
    if let (Value::String(a), Value::String(b)) = (x, y) {
        return match a.compare_lex(b) {
            std::cmp::Ordering::Less => RelationalOutcome::LessThan,
            _ => RelationalOutcome::NotLessThan,
        };
    }
    // Step 2 / 3: BigInt x String.
    if let (Value::BigInt(big), Value::String(s)) = (x, y) {
        return match BigIntValue::from_decimal(s.to_lossy_string().trim()) {
            Some(parsed) => match bigint_ops::compare(big, &parsed) {
                std::cmp::Ordering::Less => RelationalOutcome::LessThan,
                _ => RelationalOutcome::NotLessThan,
            },
            None => RelationalOutcome::Undefined,
        };
    }
    if let (Value::String(s), Value::BigInt(big)) = (x, y) {
        return match BigIntValue::from_decimal(s.to_lossy_string().trim()) {
            Some(parsed) => match bigint_ops::compare(&parsed, big) {
                std::cmp::Ordering::Less => RelationalOutcome::LessThan,
                _ => RelationalOutcome::NotLessThan,
            },
            None => RelationalOutcome::Undefined,
        };
    }

    // Numeric coercion path.
    let lnum = to_numeric_for_compare(x);
    let rnum = to_numeric_for_compare(y);
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
    /// Operand reduced to a BigInt.
    Big(BigIntValue),
}

/// §7.1.4 ToNumeric over an already-primitive Value. Mirrors
/// the spec's per-type table: Number passes through, BigInt
/// passes through, String parses via `to_number_from_string`,
/// Boolean folds to 0/1, null → 0, undefined → NaN. Symbols
/// and any non-primitive variant return `None` so the caller
/// can surface a TypeError.
///
/// Spec: <https://tc39.es/ecma262/#sec-tonumeric>
pub fn to_numeric_kind(value: &Value) -> Option<NumericKind> {
    match value {
        Value::Number(n) => Some(NumericKind::Num(*n)),
        Value::BigInt(b) => Some(NumericKind::Big(b.clone())),
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

fn to_numeric_for_compare(value: &Value) -> Option<NumericKind> {
    to_numeric_kind(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;
    use crate::string::{JsString, StringHeap};

    fn n(v: f64) -> Value {
        Value::Number(NumberValue::Double(v))
    }

    fn s(v: &str) -> Value {
        let heap = StringHeap::default();
        Value::String(JsString::from_str(v, &heap).expect("foundation heap fits the literal"))
    }

    #[test]
    fn same_value_distinguishes_signed_zeros() {
        assert!(!same_value(&n(0.0), &n(-0.0)));
        assert!(same_value_zero(&n(0.0), &n(-0.0)));
    }

    #[test]
    fn nan_equal_under_both_helpers() {
        let nan = n(f64::NAN);
        assert!(same_value(&nan, &nan));
        assert!(same_value_zero(&nan, &nan));
    }

    #[test]
    fn cross_kind_rejected() {
        assert!(!same_value(&n(1.0), &Value::Boolean(true)));
        assert!(!same_value_zero(&n(1.0), &Value::Boolean(true)));
        assert!(!same_value(&Value::Null, &Value::Undefined));
    }

    #[test]
    fn strings_compare_by_content() {
        assert!(same_value(&s("hi"), &s("hi")));
        assert!(!same_value(&s("hi"), &s("bye")));
    }

    #[test]
    fn primitives_match() {
        assert!(same_value(&Value::Undefined, &Value::Undefined));
        assert!(same_value(&Value::Null, &Value::Null));
        assert!(same_value(&Value::Boolean(true), &Value::Boolean(true)));
        assert!(!same_value(&Value::Boolean(true), &Value::Boolean(false)));
    }

    #[test]
    fn is_array_recognises_array_only() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        assert!(is_array(&Value::Array(
            crate::array::alloc_array(&mut heap).unwrap()
        )));
        assert!(!is_array(&Value::Object(
            crate::object::alloc_object(&mut heap).unwrap()
        )));
        assert!(!is_array(&Value::Undefined));
    }

    #[test]
    fn is_callable_recognises_call_shapes() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        assert!(is_callable(&Value::Function { function_id: 0 }));
        assert!(!is_callable(&Value::Object(
            crate::object::alloc_object(&mut heap).unwrap()
        )));
        assert!(!is_callable(&Value::Undefined));
    }
}
