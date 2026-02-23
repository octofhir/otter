//! Compile-time constant folding for literal expressions.
//!
//! Folds operations on constant values (numeric/string/boolean literals) at compile time,
//! eliminating runtime computation for expressions like `2 + 3`, `-1`, `!true`, etc.

use oxc_ast::ast::{BinaryOperator, Expression, UnaryOperator};

/// Maximum recursion depth for nested constant folding (e.g., `-(2 + 3)`)
const MAX_FOLD_DEPTH: usize = 10;

/// Maximum string length (in UTF-16 code units) for compile-time concatenation
const MAX_STRING_CONCAT_LEN: usize = 1024;

/// A value known at compile time.
#[derive(Debug, Clone, PartialEq)]
pub enum CompileTimeValue {
    /// A floating-point number (including -0.0, NaN, Infinity)
    Number(f64),
    /// A 32-bit integer (subset of Number, used for optimized emission)
    Int32(i32),
    /// A boolean
    Boolean(bool),
    /// A UTF-16 string
    String(Vec<u16>),
    /// null
    Null,
    /// undefined
    Undefined,
    /// A BigInt (stored as decimal string representation)
    BigInt(String),
}

impl CompileTimeValue {
    /// Convert to boolean (ES2024 7.1.2 ToBoolean)
    fn to_boolean(&self) -> bool {
        match self {
            CompileTimeValue::Number(n) => *n != 0.0 && !n.is_nan(),
            CompileTimeValue::Int32(n) => *n != 0,
            CompileTimeValue::Boolean(b) => *b,
            CompileTimeValue::String(s) => !s.is_empty(),
            CompileTimeValue::Null => false,
            CompileTimeValue::Undefined => false,
            CompileTimeValue::BigInt(s) => s != "0" && !s.is_empty(),
        }
    }

    /// Convert to f64 number (ES2024 7.1.3 ToNumber) — returns None for BigInt/Symbol
    fn to_number(&self) -> Option<f64> {
        match self {
            CompileTimeValue::Number(n) => Some(*n),
            CompileTimeValue::Int32(n) => Some(*n as f64),
            CompileTimeValue::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
            CompileTimeValue::Null => Some(0.0),
            CompileTimeValue::Undefined => Some(f64::NAN),
            CompileTimeValue::String(s) => {
                let utf8: String = char::decode_utf16(s.iter().copied())
                    .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
                    .collect();
                let trimmed = utf8.trim();
                if trimmed.is_empty() {
                    Some(0.0)
                } else {
                    trimmed.parse::<f64>().ok()
                }
            }
            CompileTimeValue::BigInt(_) => None, // TypeError at runtime
        }
    }

    /// Convert to i32 (ES2024 7.1.5 ToInt32)
    fn to_int32(&self) -> Option<i32> {
        self.to_number().map(|n| {
            if n.is_nan() || n.is_infinite() || n == 0.0 {
                0i32
            } else {
                let int = n.trunc() % 4294967296.0;
                let int = if int < 0.0 {
                    int + 4294967296.0
                } else {
                    int
                };
                if int >= 2147483648.0 {
                    (int - 4294967296.0) as i32
                } else {
                    int as i32
                }
            }
        })
    }

    /// Convert to u32 (ES2024 7.1.6 ToUint32)
    fn to_uint32(&self) -> Option<u32> {
        self.to_number().map(|n| {
            if n.is_nan() || n.is_infinite() || n == 0.0 {
                0u32
            } else {
                let int = n.trunc() % 4294967296.0;
                if int < 0.0 {
                    (int + 4294967296.0) as u32
                } else {
                    int as u32
                }
            }
        })
    }

    /// Normalize: if this is a Number that fits exactly in Int32, convert it.
    /// Preserves -0.0 as Number(-0.0).
    fn normalize(self) -> Self {
        match self {
            CompileTimeValue::Number(n) => {
                // -0.0 must stay as Number to preserve sign
                if n == 0.0 && n.is_sign_negative() {
                    return CompileTimeValue::Number(n);
                }
                if n.fract() == 0.0 && n >= i32::MIN as f64 && n <= i32::MAX as f64 {
                    CompileTimeValue::Int32(n as i32)
                } else {
                    CompileTimeValue::Number(n)
                }
            }
            other => other,
        }
    }
}

/// Extract a compile-time constant value from an AST expression (non-recursive).
pub fn try_get_literal(expr: &Expression) -> Option<CompileTimeValue> {
    match expr {
        Expression::NumericLiteral(lit) => {
            let v = lit.value;
            // -0.0 is not a NumericLiteral — it's UnaryExpression(-0)
            if v.fract() == 0.0 && v >= i32::MIN as f64 && v <= i32::MAX as f64 {
                Some(CompileTimeValue::Int32(v as i32))
            } else {
                Some(CompileTimeValue::Number(v))
            }
        }
        Expression::BooleanLiteral(lit) => Some(CompileTimeValue::Boolean(lit.value)),
        Expression::NullLiteral(_) => Some(CompileTimeValue::Null),
        Expression::StringLiteral(lit) => {
            let units: Vec<u16> = lit.value.encode_utf16().collect();
            Some(CompileTimeValue::String(units))
        }
        Expression::BigIntLiteral(lit) => Some(CompileTimeValue::BigInt(lit.value.to_string())),
        _ => None,
    }
}

/// Try to fold a constant expression (including nested unary/binary on literals).
/// Returns `None` if the expression cannot be fully folded at compile time.
pub fn try_fold_expression(expr: &Expression) -> Option<CompileTimeValue> {
    try_fold_expression_depth(expr, 0)
}

fn try_fold_expression_depth(expr: &Expression, depth: usize) -> Option<CompileTimeValue> {
    if depth > MAX_FOLD_DEPTH {
        return None;
    }

    // Try direct literal first
    if let Some(val) = try_get_literal(expr) {
        return Some(val);
    }

    match expr {
        Expression::UnaryExpression(unary) => {
            // Don't fold `typeof identifier` — may be undeclared
            if unary.operator == UnaryOperator::Typeof
                && matches!(&unary.argument, Expression::Identifier(_))
            {
                return None;
            }
            // Don't fold `delete` — has side effects
            if unary.operator == UnaryOperator::Delete {
                return None;
            }

            let operand = try_fold_expression_depth(&unary.argument, depth + 1)?;
            try_fold_unary(unary.operator, &operand)
        }
        Expression::BinaryExpression(binary) => {
            // Don't fold `instanceof` or `in` — needs runtime type info
            if matches!(
                binary.operator,
                BinaryOperator::Instanceof | BinaryOperator::In
            ) {
                return None;
            }

            let lhs = try_fold_expression_depth(&binary.left, depth + 1)?;
            let rhs = try_fold_expression_depth(&binary.right, depth + 1)?;
            try_fold_binary(binary.operator, &lhs, &rhs)
        }
        _ => None,
    }
}

/// Try to fold a unary operation on a compile-time value.
pub fn try_fold_unary(op: UnaryOperator, operand: &CompileTimeValue) -> Option<CompileTimeValue> {
    match op {
        UnaryOperator::UnaryNegation => match operand {
            CompileTimeValue::Number(n) => Some(CompileTimeValue::Number(-n).normalize()),
            CompileTimeValue::Int32(n) => {
                if *n == 0 {
                    // -0 → -0.0 (not Int32)
                    Some(CompileTimeValue::Number(-0.0))
                } else {
                    // Check for i32 overflow: -(i32::MIN) overflows
                    n.checked_neg()
                        .map(CompileTimeValue::Int32)
                        .or_else(|| Some(CompileTimeValue::Number(-(*n as f64))))
                }
            }
            CompileTimeValue::BigInt(s) => {
                if let Some(stripped) = s.strip_prefix('-') {
                    Some(CompileTimeValue::BigInt(stripped.to_string()))
                } else if s == "0" {
                    Some(CompileTimeValue::BigInt("0".to_string()))
                } else {
                    Some(CompileTimeValue::BigInt(format!("-{s}")))
                }
            }
            _ => {
                let n = operand.to_number()?;
                Some(CompileTimeValue::Number(-n).normalize())
            }
        },
        UnaryOperator::UnaryPlus => {
            // +bigint is a TypeError
            if matches!(operand, CompileTimeValue::BigInt(_)) {
                return None;
            }
            let n = operand.to_number()?;
            Some(CompileTimeValue::Number(n).normalize())
        }
        UnaryOperator::LogicalNot => Some(CompileTimeValue::Boolean(!operand.to_boolean())),
        UnaryOperator::BitwiseNot => {
            if let CompileTimeValue::BigInt(_) = operand {
                // BigInt bitwise not: ~n = -(n + 1)
                // Too complex for compile-time, skip
                return None;
            }
            let n = operand.to_int32()?;
            Some(CompileTimeValue::Int32(!n))
        }
        UnaryOperator::Typeof => {
            let type_str = match operand {
                CompileTimeValue::Number(_) | CompileTimeValue::Int32(_) => "number",
                CompileTimeValue::Boolean(_) => "boolean",
                CompileTimeValue::String(_) => "string",
                CompileTimeValue::Null => "object",
                CompileTimeValue::Undefined => "undefined",
                CompileTimeValue::BigInt(_) => "bigint",
            };
            Some(CompileTimeValue::String(
                type_str.encode_utf16().collect(),
            ))
        }
        UnaryOperator::Void => Some(CompileTimeValue::Undefined),
        UnaryOperator::Delete => None, // Side effects
    }
}

/// Try to fold a binary operation on two compile-time values.
pub fn try_fold_binary(
    op: BinaryOperator,
    lhs: &CompileTimeValue,
    rhs: &CompileTimeValue,
) -> Option<CompileTimeValue> {
    // Don't allow cross-type BigInt/Number operations (TypeError at runtime)
    let lhs_is_bigint = matches!(lhs, CompileTimeValue::BigInt(_));
    let rhs_is_bigint = matches!(rhs, CompileTimeValue::BigInt(_));
    if lhs_is_bigint != rhs_is_bigint {
        // Allow comparison operators (they work cross-type for abstract relational)
        if !matches!(
            op,
            BinaryOperator::StrictEquality | BinaryOperator::StrictInequality
        ) {
            // Non-strict equality and relational with mixed types — skip for safety
            return None;
        }
    }

    // BigInt arithmetic — skip for now (complex arbitrary-precision math)
    if lhs_is_bigint && rhs_is_bigint {
        return None;
    }

    match op {
        // Arithmetic
        BinaryOperator::Addition => fold_addition(lhs, rhs),
        BinaryOperator::Subtraction => {
            let l = lhs.to_number()?;
            let r = rhs.to_number()?;
            Some(CompileTimeValue::Number(l - r).normalize())
        }
        BinaryOperator::Multiplication => {
            let l = lhs.to_number()?;
            let r = rhs.to_number()?;
            Some(CompileTimeValue::Number(l * r).normalize())
        }
        BinaryOperator::Division => {
            let l = lhs.to_number()?;
            let r = rhs.to_number()?;
            Some(CompileTimeValue::Number(l / r).normalize())
        }
        BinaryOperator::Remainder => {
            let l = lhs.to_number()?;
            let r = rhs.to_number()?;
            Some(CompileTimeValue::Number(l % r).normalize())
        }
        BinaryOperator::Exponential => {
            let l = lhs.to_number()?;
            let r = rhs.to_number()?;
            Some(CompileTimeValue::Number(l.powf(r)).normalize())
        }

        // Bitwise (convert to int32/uint32 first)
        BinaryOperator::BitwiseAnd => {
            let l = lhs.to_int32()?;
            let r = rhs.to_int32()?;
            Some(CompileTimeValue::Int32(l & r))
        }
        BinaryOperator::BitwiseOR => {
            let l = lhs.to_int32()?;
            let r = rhs.to_int32()?;
            Some(CompileTimeValue::Int32(l | r))
        }
        BinaryOperator::BitwiseXOR => {
            let l = lhs.to_int32()?;
            let r = rhs.to_int32()?;
            Some(CompileTimeValue::Int32(l ^ r))
        }
        BinaryOperator::ShiftLeft => {
            let l = lhs.to_int32()?;
            let r = rhs.to_uint32()? & 0x1f;
            Some(CompileTimeValue::Int32(l << r))
        }
        BinaryOperator::ShiftRight => {
            let l = lhs.to_int32()?;
            let r = rhs.to_uint32()? & 0x1f;
            Some(CompileTimeValue::Int32(l >> r))
        }
        BinaryOperator::ShiftRightZeroFill => {
            let l = lhs.to_uint32()?;
            let r = rhs.to_uint32()? & 0x1f;
            let result = l >> r;
            // Result is uint32, which may not fit in i32
            if result <= i32::MAX as u32 {
                Some(CompileTimeValue::Int32(result as i32))
            } else {
                Some(CompileTimeValue::Number(result as f64))
            }
        }

        // Comparisons
        BinaryOperator::StrictEquality => fold_strict_equality(lhs, rhs).map(CompileTimeValue::Boolean),
        BinaryOperator::StrictInequality => fold_strict_equality(lhs, rhs).map(|eq| CompileTimeValue::Boolean(!eq)),
        BinaryOperator::LessThan => fold_relational_number(lhs, rhs, |l, r| l < r),
        BinaryOperator::LessEqualThan => fold_relational_number(lhs, rhs, |l, r| l <= r),
        BinaryOperator::GreaterThan => fold_relational_number(lhs, rhs, |l, r| l > r),
        BinaryOperator::GreaterEqualThan => fold_relational_number(lhs, rhs, |l, r| l >= r),

        // Abstract equality — too complex (type coercion rules), skip
        BinaryOperator::Equality | BinaryOperator::Inequality => None,

        // instanceof / in — need runtime
        BinaryOperator::Instanceof | BinaryOperator::In => None,
    }
}

/// Addition: handles string concatenation and numeric addition
fn fold_addition(lhs: &CompileTimeValue, rhs: &CompileTimeValue) -> Option<CompileTimeValue> {
    // If either operand is a string, do concatenation
    if let CompileTimeValue::String(l) = lhs {
        let r_str = compile_time_to_string(rhs)?;
        let total_len = l.len() + r_str.len();
        if total_len > MAX_STRING_CONCAT_LEN {
            return None;
        }
        let mut result = l.clone();
        result.extend_from_slice(&r_str);
        return Some(CompileTimeValue::String(result));
    }
    if let CompileTimeValue::String(r) = rhs {
        let l_str = compile_time_to_string(lhs)?;
        let total_len = l_str.len() + r.len();
        if total_len > MAX_STRING_CONCAT_LEN {
            return None;
        }
        let mut result = l_str;
        result.extend_from_slice(r);
        return Some(CompileTimeValue::String(result));
    }

    // Numeric addition
    let l = lhs.to_number()?;
    let r = rhs.to_number()?;
    Some(CompileTimeValue::Number(l + r).normalize())
}

/// Convert a compile-time value to its string representation (ES2024 7.1.12 ToString)
fn compile_time_to_string(val: &CompileTimeValue) -> Option<Vec<u16>> {
    let s: String = match val {
        CompileTimeValue::Number(n) => format_number(*n),
        CompileTimeValue::Int32(n) => n.to_string(),
        CompileTimeValue::Boolean(b) => b.to_string(),
        CompileTimeValue::Null => "null".to_string(),
        CompileTimeValue::Undefined => "undefined".to_string(),
        CompileTimeValue::String(s) => return Some(s.clone()),
        CompileTimeValue::BigInt(s) => s.clone(),
    };
    Some(s.encode_utf16().collect())
}

/// Format a number to string following ES2024 7.1.12.1 Number::toString
fn format_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n == 0.0 {
        "0".to_string()
    } else if n.is_infinite() {
        if n > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else {
        // Use Rust's Display which closely matches ES number formatting for most cases
        let s = format!("{n}");
        // Remove trailing ".0" if present (JS doesn't show it)
        // Actually Rust format! doesn't add .0 for f64 by default with {}
        s
    }
}

/// Strict equality comparison between compile-time values
fn fold_strict_equality(lhs: &CompileTimeValue, rhs: &CompileTimeValue) -> Option<bool> {
    // Different types → false (except Int32/Number are same type)
    match (lhs, rhs) {
        (CompileTimeValue::Number(l), CompileTimeValue::Number(r)) => Some(l == r),
        (CompileTimeValue::Number(l), CompileTimeValue::Int32(r)) => Some(*l == *r as f64),
        (CompileTimeValue::Int32(l), CompileTimeValue::Number(r)) => Some(*l as f64 == *r),
        (CompileTimeValue::Int32(l), CompileTimeValue::Int32(r)) => Some(l == r),
        (CompileTimeValue::Boolean(l), CompileTimeValue::Boolean(r)) => Some(l == r),
        (CompileTimeValue::String(l), CompileTimeValue::String(r)) => Some(l == r),
        (CompileTimeValue::Null, CompileTimeValue::Null) => Some(true),
        (CompileTimeValue::Undefined, CompileTimeValue::Undefined) => Some(true),
        (CompileTimeValue::BigInt(l), CompileTimeValue::BigInt(r)) => Some(l == r),
        // Different types
        _ => Some(false),
    }
}

/// Relational comparison for numeric operands
fn fold_relational_number(
    lhs: &CompileTimeValue,
    rhs: &CompileTimeValue,
    cmp: impl Fn(f64, f64) -> bool,
) -> Option<CompileTimeValue> {
    // Only fold when both are numeric types
    let l = lhs.to_number()?;
    let r = rhs.to_number()?;
    // NaN comparisons return false (except for != which we don't fold here)
    if l.is_nan() || r.is_nan() {
        // For <, <=, >, >= with NaN: result is always false
        Some(CompileTimeValue::Boolean(false))
    } else {
        Some(CompileTimeValue::Boolean(cmp(l, r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unary_negation() {
        assert_eq!(
            try_fold_unary(UnaryOperator::UnaryNegation, &CompileTimeValue::Int32(5)),
            Some(CompileTimeValue::Int32(-5))
        );
        assert_eq!(
            try_fold_unary(UnaryOperator::UnaryNegation, &CompileTimeValue::Int32(0)),
            Some(CompileTimeValue::Number(-0.0))
        );
        assert_eq!(
            try_fold_unary(
                UnaryOperator::UnaryNegation,
                &CompileTimeValue::Number(3.14)
            ),
            Some(CompileTimeValue::Number(-3.14))
        );
    }

    #[test]
    fn test_unary_plus() {
        assert_eq!(
            try_fold_unary(UnaryOperator::UnaryPlus, &CompileTimeValue::Boolean(true)),
            Some(CompileTimeValue::Int32(1))
        );
        assert_eq!(
            try_fold_unary(UnaryOperator::UnaryPlus, &CompileTimeValue::Null),
            Some(CompileTimeValue::Int32(0))
        );
        // +BigInt is TypeError → None
        assert_eq!(
            try_fold_unary(
                UnaryOperator::UnaryPlus,
                &CompileTimeValue::BigInt("42".into())
            ),
            None
        );
    }

    #[test]
    fn test_logical_not() {
        assert_eq!(
            try_fold_unary(
                UnaryOperator::LogicalNot,
                &CompileTimeValue::Boolean(true)
            ),
            Some(CompileTimeValue::Boolean(false))
        );
        assert_eq!(
            try_fold_unary(UnaryOperator::LogicalNot, &CompileTimeValue::Int32(0)),
            Some(CompileTimeValue::Boolean(true))
        );
        assert_eq!(
            try_fold_unary(
                UnaryOperator::LogicalNot,
                &CompileTimeValue::String(vec![])
            ),
            Some(CompileTimeValue::Boolean(true))
        );
    }

    #[test]
    fn test_bitwise_not() {
        assert_eq!(
            try_fold_unary(UnaryOperator::BitwiseNot, &CompileTimeValue::Int32(0)),
            Some(CompileTimeValue::Int32(-1))
        );
        assert_eq!(
            try_fold_unary(UnaryOperator::BitwiseNot, &CompileTimeValue::Int32(-1)),
            Some(CompileTimeValue::Int32(0))
        );
    }

    #[test]
    fn test_typeof_constants() {
        assert_eq!(
            try_fold_unary(UnaryOperator::Typeof, &CompileTimeValue::Int32(42)),
            Some(CompileTimeValue::String("number".encode_utf16().collect()))
        );
        assert_eq!(
            try_fold_unary(UnaryOperator::Typeof, &CompileTimeValue::Null),
            Some(CompileTimeValue::String("object".encode_utf16().collect()))
        );
        assert_eq!(
            try_fold_unary(UnaryOperator::Typeof, &CompileTimeValue::Undefined),
            Some(CompileTimeValue::String(
                "undefined".encode_utf16().collect()
            ))
        );
    }

    #[test]
    fn test_binary_arithmetic() {
        // 2 + 3 = 5
        assert_eq!(
            try_fold_binary(
                BinaryOperator::Addition,
                &CompileTimeValue::Int32(2),
                &CompileTimeValue::Int32(3)
            ),
            Some(CompileTimeValue::Int32(5))
        );
        // 10 / 3 = 3.333...
        let result = try_fold_binary(
            BinaryOperator::Division,
            &CompileTimeValue::Int32(10),
            &CompileTimeValue::Int32(3),
        );
        match result {
            Some(CompileTimeValue::Number(n)) => assert!((n - 10.0 / 3.0).abs() < f64::EPSILON),
            other => panic!("Expected Number, got {other:?}"),
        }
        // 2 ** 10 = 1024
        assert_eq!(
            try_fold_binary(
                BinaryOperator::Exponential,
                &CompileTimeValue::Int32(2),
                &CompileTimeValue::Int32(10)
            ),
            Some(CompileTimeValue::Int32(1024))
        );
    }

    #[test]
    fn test_string_concat() {
        assert_eq!(
            try_fold_binary(
                BinaryOperator::Addition,
                &CompileTimeValue::String("hello ".encode_utf16().collect()),
                &CompileTimeValue::String("world".encode_utf16().collect())
            ),
            Some(CompileTimeValue::String(
                "hello world".encode_utf16().collect()
            ))
        );
        // String + number
        assert_eq!(
            try_fold_binary(
                BinaryOperator::Addition,
                &CompileTimeValue::String("x=".encode_utf16().collect()),
                &CompileTimeValue::Int32(42)
            ),
            Some(CompileTimeValue::String("x=42".encode_utf16().collect()))
        );
    }

    #[test]
    fn test_bitwise_operations() {
        assert_eq!(
            try_fold_binary(
                BinaryOperator::BitwiseAnd,
                &CompileTimeValue::Int32(0xFF),
                &CompileTimeValue::Int32(0x0F)
            ),
            Some(CompileTimeValue::Int32(0x0F))
        );
        assert_eq!(
            try_fold_binary(
                BinaryOperator::ShiftLeft,
                &CompileTimeValue::Int32(1),
                &CompileTimeValue::Int32(8)
            ),
            Some(CompileTimeValue::Int32(256))
        );
        // >>> produces uint32
        assert_eq!(
            try_fold_binary(
                BinaryOperator::ShiftRightZeroFill,
                &CompileTimeValue::Int32(-1),
                &CompileTimeValue::Int32(0)
            ),
            Some(CompileTimeValue::Number(4294967295.0))
        );
    }

    #[test]
    fn test_strict_equality() {
        assert_eq!(
            try_fold_binary(
                BinaryOperator::StrictEquality,
                &CompileTimeValue::Int32(1),
                &CompileTimeValue::Int32(1)
            ),
            Some(CompileTimeValue::Boolean(true))
        );
        assert_eq!(
            try_fold_binary(
                BinaryOperator::StrictEquality,
                &CompileTimeValue::Int32(1),
                &CompileTimeValue::Boolean(true)
            ),
            Some(CompileTimeValue::Boolean(false))
        );
        // NaN !== NaN
        assert_eq!(
            try_fold_binary(
                BinaryOperator::StrictEquality,
                &CompileTimeValue::Number(f64::NAN),
                &CompileTimeValue::Number(f64::NAN)
            ),
            Some(CompileTimeValue::Boolean(false))
        );
    }

    #[test]
    fn test_relational() {
        assert_eq!(
            try_fold_binary(
                BinaryOperator::LessThan,
                &CompileTimeValue::Int32(1),
                &CompileTimeValue::Int32(2)
            ),
            Some(CompileTimeValue::Boolean(true))
        );
        // NaN comparisons are always false
        assert_eq!(
            try_fold_binary(
                BinaryOperator::LessThan,
                &CompileTimeValue::Number(f64::NAN),
                &CompileTimeValue::Int32(1)
            ),
            Some(CompileTimeValue::Boolean(false))
        );
    }

    #[test]
    fn test_negative_zero() {
        // -0 should be Number(-0.0), not Int32(0)
        let result =
            try_fold_unary(UnaryOperator::UnaryNegation, &CompileTimeValue::Int32(0)).unwrap();
        match result {
            CompileTimeValue::Number(n) => {
                assert!(n == 0.0 && n.is_sign_negative());
            }
            other => panic!("Expected Number(-0.0), got {other:?}"),
        }
    }

    #[test]
    fn test_cross_type_bigint_number_rejected() {
        // BigInt + Number → None (TypeError)
        assert_eq!(
            try_fold_binary(
                BinaryOperator::Addition,
                &CompileTimeValue::BigInt("1".into()),
                &CompileTimeValue::Int32(2)
            ),
            None
        );
    }

    #[test]
    fn test_void_folding() {
        assert_eq!(
            try_fold_unary(UnaryOperator::Void, &CompileTimeValue::Int32(42)),
            Some(CompileTimeValue::Undefined)
        );
    }
}
