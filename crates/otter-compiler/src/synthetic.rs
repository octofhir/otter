//! Literal interning and key-formatting helpers shared by expression lowering.
//!
//! # Contents
//! - UTF-16 string interning
//! - lone-surrogate key detection
//! - numeric literal property-key formatting
//!
//! # Invariants
//! - OXC strings retain their exact UTF-16 code units.
//!
//! # See also
//! - `module_state` and `expr`

/// Intern an OXC JavaScript string without replacing unpaired surrogates.
pub(crate) fn intern_js_string(cx: &mut crate::Compiler, value: oxc_str::JSStr<'_>) -> u32 {
    match value.as_str() {
        Some(text) => cx.intern_string_constant(text),
        None => cx.intern_utf16_string_constant(value.encode_utf16().collect()),
    }
}

/// Whether `key` is a string literal holding an unpaired surrogate. No UTF-8 static name
/// spells it: such a key lowers like a computed key, whose literal decodes
/// to the exact code units.
pub(crate) fn is_lone_surrogate_key(key: &oxc_ast::ast::PropertyKey<'_>) -> bool {
    matches!(key, oxc_ast::ast::PropertyKey::StringLiteral(lit) if lit.value.has_lone_surrogate())
}

/// The settled property-key spelling of a `NumericLiteral` per
/// §6.1.6.1.13 `Number::toString`: the bare integer string ("1", not
/// "1.0") for an integral value inside the exactly-representable range.
///
/// `None` for everything else — a fractional value, a magnitude past
/// the integer range, `Infinity`, `NaN`. Those keys are lowered through
/// the computed-key path instead, where the runtime's §7.1.19
/// ToPropertyKey formats the number, so the engine keeps exactly one
/// Number-to-String implementation.
pub(crate) fn numeric_literal_to_property_key(n: f64) -> Option<String> {
    const EXACT_INTEGER_LIMIT: f64 = 9_007_199_254_740_992.0;
    if n.is_finite() && n.fract() == 0.0 && n.abs() <= EXACT_INTEGER_LIMIT {
        // `-0` stringifies as "0" (§6.1.6.1.13 step 2).
        return Some(format!("{}", n as i64));
    }
    None
}
