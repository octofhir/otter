//! `f64_to_int32`, `f64_to_uint32`, `parse_string_to_number`, and
//! `canonical_string_exotic_index` — numeric / string-index conversion
//! helpers from ECMA-262 §7.1.6, §7.1.7, and §9.4.3.

/// ES spec 7.1.4.1 StringToNumber — parses a string to a number.
/// ES spec 7.1.6 ToInt32(argument).
pub(crate) fn f64_to_int32(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    // Step 3-5: modulo 2^32, then adjust to signed range.
    let i = (n.trunc() % 4_294_967_296.0) as i64;
    let i = if i < 0 { i + 4_294_967_296 } else { i };
    if i >= 2_147_483_648 {
        (i - 4_294_967_296) as i32
    } else {
        i as i32
    }
}

/// ES spec 7.1.7 ToUint32(argument).
pub(crate) fn f64_to_uint32(n: f64) -> u32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    let i = (n.trunc() % 4_294_967_296.0) as i64;
    if i < 0 {
        (i + 4_294_967_296) as u32
    } else {
        i as u32
    }
}

pub(super) fn parse_string_to_number(s: &str) -> f64 {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    match trimmed {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ => parse_non_decimal_integer_literal(trimmed)
            .map(|value| value as f64)
            .unwrap_or_else(|| trimmed.parse::<f64>().unwrap_or(f64::NAN)),
    }
}

fn parse_non_decimal_integer_literal(s: &str) -> Option<u64> {
    let (digits, radix) = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .map(|digits| (digits, 16))
        .or_else(|| {
            s.strip_prefix("0o")
                .or_else(|| s.strip_prefix("0O"))
                .map(|digits| (digits, 8))
        })
        .or_else(|| {
            s.strip_prefix("0b")
                .or_else(|| s.strip_prefix("0B"))
                .map(|digits| (digits, 2))
        })?;
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

pub(super) fn canonical_string_exotic_index(property_name: &str) -> Option<usize> {
    let index = property_name.parse::<u32>().ok()?;
    if index == u32::MAX || index.to_string() != property_name {
        return None;
    }
    Some(index as usize)
}
