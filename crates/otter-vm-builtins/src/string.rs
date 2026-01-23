//! String built-in
//!
//! Provides String.prototype methods and String constructor methods:
//! - charAt, charCodeAt, codePointAt
//! - concat, includes, indexOf, lastIndexOf
//! - slice, substring, split
//! - toLowerCase, toUpperCase, toLocaleLowerCase, toLocaleUpperCase
//! - trim, trimStart, trimEnd
//! - replace, replaceAll
//! - startsWith, endsWith
//! - repeat, padStart, padEnd
//! - at (ES2022)
//! - normalize
//! - isWellFormed, toWellFormed (ES2024)
//! - String.fromCharCode, String.fromCodePoint

use otter_vm_runtime::{Op, op_sync};
use serde_json::{Value as JsonValue, json};

/// Get String ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // String.prototype methods
        op_sync("__String_charAt", string_char_at),
        op_sync("__String_charCodeAt", string_char_code_at),
        op_sync("__String_codePointAt", string_code_point_at),
        op_sync("__String_concat", string_concat),
        op_sync("__String_includes", string_includes),
        op_sync("__String_indexOf", string_index_of),
        op_sync("__String_lastIndexOf", string_last_index_of),
        op_sync("__String_slice", string_slice),
        op_sync("__String_substring", string_substring),
        op_sync("__String_split", string_split),
        op_sync("__String_toLowerCase", string_to_lower_case),
        op_sync("__String_toUpperCase", string_to_upper_case),
        op_sync("__String_toLocaleLowerCase", string_to_locale_lower_case),
        op_sync("__String_toLocaleUpperCase", string_to_locale_upper_case),
        op_sync("__String_trim", string_trim),
        op_sync("__String_trimStart", string_trim_start),
        op_sync("__String_trimEnd", string_trim_end),
        op_sync("__String_replace", string_replace),
        op_sync("__String_replaceAll", string_replace_all),
        op_sync("__String_startsWith", string_starts_with),
        op_sync("__String_endsWith", string_ends_with),
        op_sync("__String_repeat", string_repeat),
        op_sync("__String_padStart", string_pad_start),
        op_sync("__String_padEnd", string_pad_end),
        op_sync("__String_length", string_length),
        op_sync("__String_at", string_at),
        op_sync("__String_normalize", string_normalize),
        op_sync("__String_isWellFormed", string_is_well_formed),
        op_sync("__String_toWellFormed", string_to_well_formed),
        op_sync("__String_localeCompare", string_locale_compare),
        // Static methods
        op_sync("__String_fromCharCode", string_from_char_code),
        op_sync("__String_fromCodePoint", string_from_code_point),
    ]
}

fn get_string(args: &[JsonValue], index: usize) -> String {
    args.get(index)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn get_int(args: &[JsonValue], index: usize) -> Option<i64> {
    args.get(index).and_then(|v| v.as_i64())
}

fn get_uint(args: &[JsonValue], index: usize) -> Option<u64> {
    args.get(index).and_then(|v| v.as_u64())
}

// =============================================================================
// String.prototype methods
// =============================================================================

fn string_char_at(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let index = get_uint(args, 1).unwrap_or(0) as usize;
    let ch = s
        .chars()
        .nth(index)
        .map(|c| c.to_string())
        .unwrap_or_default();
    Ok(json!(ch))
}

fn string_char_code_at(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let index = get_uint(args, 1).unwrap_or(0) as usize;
    // charCodeAt returns UTF-16 code unit
    let utf16: Vec<u16> = s.encode_utf16().collect();
    if index < utf16.len() {
        Ok(json!(utf16[index]))
    } else {
        // Return NaN for out of bounds (represented as null in JSON)
        Ok(JsonValue::Null)
    }
}

fn string_code_point_at(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let index = get_uint(args, 1).unwrap_or(0) as usize;
    match s.chars().nth(index) {
        Some(c) => Ok(json!(c as u32)),
        None => Ok(JsonValue::Null), // undefined for out of bounds
    }
}

fn string_concat(args: &[JsonValue]) -> Result<JsonValue, String> {
    let result: String = args.iter().filter_map(|v| v.as_str()).collect();
    Ok(json!(result))
}

fn string_includes(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let position = get_uint(args, 2).unwrap_or(0) as usize;

    if position >= s.chars().count() {
        return Ok(json!(false));
    }

    let substring: String = s.chars().skip(position).collect();
    Ok(json!(substring.contains(&search)))
}

fn string_index_of(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let position = get_uint(args, 2).unwrap_or(0) as usize;

    if position >= s.chars().count() {
        return Ok(json!(-1));
    }

    // Work with character indices, not byte indices
    let chars: Vec<char> = s.chars().collect();
    let search_chars: Vec<char> = search.chars().collect();

    if search_chars.is_empty() {
        return Ok(json!(position as i64));
    }

    for i in position..=chars.len().saturating_sub(search_chars.len()) {
        if chars[i..i + search_chars.len()] == search_chars[..] {
            return Ok(json!(i as i64));
        }
    }

    Ok(json!(-1))
}

fn string_last_index_of(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let position = get_uint(args, 2);

    let chars: Vec<char> = s.chars().collect();
    let search_chars: Vec<char> = search.chars().collect();

    if search_chars.is_empty() {
        return Ok(json!(chars.len() as i64));
    }

    let max_start = position
        .map(|p| (p as usize).min(chars.len().saturating_sub(search_chars.len())))
        .unwrap_or_else(|| chars.len().saturating_sub(search_chars.len()));

    for i in (0..=max_start).rev() {
        if i + search_chars.len() <= chars.len()
            && chars[i..i + search_chars.len()] == search_chars[..]
        {
            return Ok(json!(i as i64));
        }
    }

    Ok(json!(-1))
}

fn string_slice(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;

    let start = get_int(args, 1).unwrap_or(0);
    let end = get_int(args, 2);

    let start = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;

    let end = match end {
        Some(e) if e < 0 => (len + e).max(0) as usize,
        Some(e) => e.min(len) as usize,
        None => len as usize,
    };

    let result: String = chars
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    Ok(json!(result))
}

fn string_substring(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();

    let start = get_int(args, 1).unwrap_or(0).max(0) as usize;
    let end = get_int(args, 2).map(|e| e.max(0) as usize).unwrap_or(len);

    // substring swaps arguments if start > end
    let (start, end) = (start.min(end).min(len), start.max(end).min(len));
    let result: String = chars.iter().skip(start).take(end - start).collect();
    Ok(json!(result))
}

fn string_split(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let separator = args.get(1);
    let limit = get_uint(args, 2);

    // If separator is undefined, return array with original string
    if separator.is_none() || separator == Some(&JsonValue::Null) {
        return Ok(json!([s]));
    }

    let separator = get_string(args, 1);

    let parts: Vec<JsonValue> = if separator.is_empty() {
        s.chars().map(|c| json!(c.to_string())).collect()
    } else {
        s.split(&separator).map(|p| json!(p)).collect()
    };

    let parts = match limit {
        Some(l) => parts.into_iter().take(l as usize).collect(),
        None => parts,
    };

    Ok(JsonValue::Array(parts))
}

fn string_to_lower_case(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    Ok(json!(s.to_lowercase()))
}

fn string_to_upper_case(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    Ok(json!(s.to_uppercase()))
}

fn string_to_locale_lower_case(args: &[JsonValue]) -> Result<JsonValue, String> {
    // Simplified: same as toLowerCase for now (no locale support)
    let s = get_string(args, 0);
    Ok(json!(s.to_lowercase()))
}

fn string_to_locale_upper_case(args: &[JsonValue]) -> Result<JsonValue, String> {
    // Simplified: same as toUpperCase for now (no locale support)
    let s = get_string(args, 0);
    Ok(json!(s.to_uppercase()))
}

fn string_trim(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    Ok(json!(s.trim()))
}

fn string_trim_start(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    Ok(json!(s.trim_start()))
}

fn string_trim_end(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    Ok(json!(s.trim_end()))
}

fn string_replace(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let replacement = get_string(args, 2);
    Ok(json!(s.replacen(&search, &replacement, 1)))
}

fn string_replace_all(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let replacement = get_string(args, 2);
    Ok(json!(s.replace(&search, &replacement)))
}

fn string_starts_with(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let position = get_uint(args, 2).unwrap_or(0) as usize;

    let chars: Vec<char> = s.chars().collect();
    if position > chars.len() {
        return Ok(json!(false));
    }

    let substring: String = chars.iter().skip(position).collect();
    Ok(json!(substring.starts_with(&search)))
}

fn string_ends_with(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let end_position = get_uint(args, 2);

    let chars: Vec<char> = s.chars().collect();
    let end_pos = end_position.map(|e| e as usize).unwrap_or(chars.len());
    let substring: String = chars.iter().take(end_pos).collect();
    Ok(json!(substring.ends_with(&search)))
}

fn string_repeat(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let count = get_uint(args, 1).unwrap_or(0) as usize;

    // Check for RangeError conditions
    if count > 1_000_000 {
        return Err("Invalid count value".to_string());
    }

    Ok(json!(s.repeat(count)))
}

fn string_pad_start(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let target_length = get_uint(args, 1).unwrap_or(0) as usize;
    let pad_string = args.get(2).and_then(|v| v.as_str()).unwrap_or(" ");

    let char_count = s.chars().count();
    if char_count >= target_length || pad_string.is_empty() {
        return Ok(json!(s));
    }

    let pad_len = target_length - char_count;
    let pad: String = pad_string.chars().cycle().take(pad_len).collect();
    Ok(json!(format!("{}{}", pad, s)))
}

fn string_pad_end(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let target_length = get_uint(args, 1).unwrap_or(0) as usize;
    let pad_string = args.get(2).and_then(|v| v.as_str()).unwrap_or(" ");

    let char_count = s.chars().count();
    if char_count >= target_length || pad_string.is_empty() {
        return Ok(json!(s));
    }

    let pad_len = target_length - char_count;
    let pad: String = pad_string.chars().cycle().take(pad_len).collect();
    Ok(json!(format!("{}{}", s, pad)))
}

fn string_length(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    // UTF-16 length for JS semantics
    let len = s.encode_utf16().count();
    Ok(json!(len))
}

/// ES2022 String.prototype.at()
fn string_at(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let index = get_int(args, 1).unwrap_or(0);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;

    let actual_index = if index < 0 { len + index } else { index };

    if actual_index < 0 || actual_index >= len {
        return Ok(JsonValue::Null); // undefined
    }

    Ok(json!(chars[actual_index as usize].to_string()))
}

/// String.prototype.normalize()
fn string_normalize(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let form = args.get(1).and_then(|v| v.as_str()).unwrap_or("NFC");

    // Rust's unicode-normalization crate would be needed for full support
    // For now, return the string as-is (NFC is often no-op for ASCII)
    match form {
        "NFC" | "NFD" | "NFKC" | "NFKD" => Ok(json!(s)),
        _ => Err(format!("Invalid normalization form: {}", form)),
    }
}

/// ES2024 String.prototype.isWellFormed()
fn string_is_well_formed(args: &[JsonValue]) -> Result<JsonValue, String> {
    // In Rust, String is always valid UTF-8, which means no unpaired surrogates
    // can exist. Therefore, any Rust String is always "well-formed" per ES2024.
    let _s = get_string(args, 0);
    Ok(json!(true))
}

/// ES2024 String.prototype.toWellFormed()
fn string_to_well_formed(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    // In Rust, String is always valid UTF-8
    // This would replace lone surrogates with U+FFFD
    // Since Rust strings can't have lone surrogates, just return the string
    Ok(json!(s))
}

/// String.prototype.localeCompare()
fn string_locale_compare(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let other = get_string(args, 1);
    // Simplified: lexicographic comparison (no locale support)
    let result = match s.cmp(&other) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(json!(result))
}

// =============================================================================
// Static methods
// =============================================================================

fn string_from_char_code(args: &[JsonValue]) -> Result<JsonValue, String> {
    let result: String = args
        .iter()
        .filter_map(|v| v.as_u64())
        .filter_map(|code| {
            // UTF-16 code unit to char
            if code <= 0xFFFF {
                char::from_u32(code as u32)
            } else {
                None
            }
        })
        .collect();
    Ok(json!(result))
}

fn string_from_code_point(args: &[JsonValue]) -> Result<JsonValue, String> {
    let mut result = String::new();
    for v in args {
        if let Some(code) = v.as_u64() {
            if code > 0x10FFFF {
                return Err(format!("Invalid code point: {}", code));
            }
            if let Some(c) = char::from_u32(code as u32) {
                result.push(c);
            } else {
                return Err(format!("Invalid code point: {}", code));
            }
        }
    }
    Ok(json!(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_char_at() {
        let result = string_char_at(&[json!("hello"), json!(1)]).unwrap();
        assert_eq!(result, json!("e"));

        let result = string_char_at(&[json!("hello"), json!(10)]).unwrap();
        assert_eq!(result, json!(""));
    }

    #[test]
    fn test_char_code_at() {
        let result = string_char_code_at(&[json!("A"), json!(0)]).unwrap();
        assert_eq!(result, json!(65));
    }

    #[test]
    fn test_code_point_at() {
        let result = string_code_point_at(&[json!("😀"), json!(0)]).unwrap();
        assert_eq!(result, json!(128512)); // U+1F600
    }

    #[test]
    fn test_concat() {
        let result = string_concat(&[json!("hello"), json!(" "), json!("world")]).unwrap();
        assert_eq!(result, json!("hello world"));
    }

    #[test]
    fn test_includes() {
        let result = string_includes(&[json!("hello world"), json!("world")]).unwrap();
        assert_eq!(result, json!(true));

        let result = string_includes(&[json!("hello world"), json!("foo")]).unwrap();
        assert_eq!(result, json!(false));

        let result = string_includes(&[json!("hello world"), json!("hello"), json!(1)]).unwrap();
        assert_eq!(result, json!(false));
    }

    #[test]
    fn test_index_of() {
        let result = string_index_of(&[json!("hello world"), json!("world")]).unwrap();
        assert_eq!(result, json!(6));

        let result = string_index_of(&[json!("hello world"), json!("foo")]).unwrap();
        assert_eq!(result, json!(-1));
    }

    #[test]
    fn test_last_index_of() {
        let result = string_last_index_of(&[json!("hello hello"), json!("hello")]).unwrap();
        assert_eq!(result, json!(6));
    }

    #[test]
    fn test_slice() {
        let result = string_slice(&[json!("hello"), json!(1), json!(4)]).unwrap();
        assert_eq!(result, json!("ell"));

        let result = string_slice(&[json!("hello"), json!(-2)]).unwrap();
        assert_eq!(result, json!("lo"));
    }

    #[test]
    fn test_substring() {
        let result = string_substring(&[json!("hello"), json!(1), json!(4)]).unwrap();
        assert_eq!(result, json!("ell"));

        // substring swaps if start > end
        let result = string_substring(&[json!("hello"), json!(4), json!(1)]).unwrap();
        assert_eq!(result, json!("ell"));
    }

    #[test]
    fn test_split() {
        let result = string_split(&[json!("a,b,c"), json!(",")]).unwrap();
        assert_eq!(result, json!(["a", "b", "c"]));

        let result = string_split(&[json!("abc"), json!("")]).unwrap();
        assert_eq!(result, json!(["a", "b", "c"]));

        let result = string_split(&[json!("a,b,c"), json!(","), json!(2)]).unwrap();
        assert_eq!(result, json!(["a", "b"]));
    }

    #[test]
    fn test_to_lower_case() {
        let result = string_to_lower_case(&[json!("HELLO")]).unwrap();
        assert_eq!(result, json!("hello"));
    }

    #[test]
    fn test_to_upper_case() {
        let result = string_to_upper_case(&[json!("hello")]).unwrap();
        assert_eq!(result, json!("HELLO"));
    }

    #[test]
    fn test_trim() {
        let result = string_trim(&[json!("  hello  ")]).unwrap();
        assert_eq!(result, json!("hello"));
    }

    #[test]
    fn test_trim_start() {
        let result = string_trim_start(&[json!("  hello  ")]).unwrap();
        assert_eq!(result, json!("hello  "));
    }

    #[test]
    fn test_trim_end() {
        let result = string_trim_end(&[json!("  hello  ")]).unwrap();
        assert_eq!(result, json!("  hello"));
    }

    #[test]
    fn test_replace() {
        let result = string_replace(&[json!("hello hello"), json!("hello"), json!("hi")]).unwrap();
        assert_eq!(result, json!("hi hello"));
    }

    #[test]
    fn test_replace_all() {
        let result =
            string_replace_all(&[json!("hello hello"), json!("hello"), json!("hi")]).unwrap();
        assert_eq!(result, json!("hi hi"));
    }

    #[test]
    fn test_starts_with() {
        let result = string_starts_with(&[json!("hello"), json!("he")]).unwrap();
        assert_eq!(result, json!(true));

        let result = string_starts_with(&[json!("hello"), json!("lo")]).unwrap();
        assert_eq!(result, json!(false));
    }

    #[test]
    fn test_ends_with() {
        let result = string_ends_with(&[json!("hello"), json!("lo")]).unwrap();
        assert_eq!(result, json!(true));

        let result = string_ends_with(&[json!("hello"), json!("he")]).unwrap();
        assert_eq!(result, json!(false));
    }

    #[test]
    fn test_repeat() {
        let result = string_repeat(&[json!("ab"), json!(3)]).unwrap();
        assert_eq!(result, json!("ababab"));
    }

    #[test]
    fn test_pad_start() {
        let result = string_pad_start(&[json!("5"), json!(3), json!("0")]).unwrap();
        assert_eq!(result, json!("005"));
    }

    #[test]
    fn test_pad_end() {
        let result = string_pad_end(&[json!("5"), json!(3), json!("0")]).unwrap();
        assert_eq!(result, json!("500"));
    }

    #[test]
    fn test_length() {
        let result = string_length(&[json!("hello")]).unwrap();
        assert_eq!(result, json!(5));

        // Emoji has length 2 in UTF-16
        let result = string_length(&[json!("😀")]).unwrap();
        assert_eq!(result, json!(2));
    }

    #[test]
    fn test_at() {
        let result = string_at(&[json!("hello"), json!(1)]).unwrap();
        assert_eq!(result, json!("e"));

        let result = string_at(&[json!("hello"), json!(-1)]).unwrap();
        assert_eq!(result, json!("o"));
    }

    #[test]
    fn test_from_char_code() {
        let result = string_from_char_code(&[json!(72), json!(105)]).unwrap();
        assert_eq!(result, json!("Hi"));
    }

    #[test]
    fn test_from_code_point() {
        let result = string_from_code_point(&[json!(128512)]).unwrap();
        assert_eq!(result, json!("😀"));
    }

    #[test]
    fn test_locale_compare() {
        let result = string_locale_compare(&[json!("a"), json!("b")]).unwrap();
        assert_eq!(result, json!(-1));

        let result = string_locale_compare(&[json!("b"), json!("a")]).unwrap();
        assert_eq!(result, json!(1));

        let result = string_locale_compare(&[json!("a"), json!("a")]).unwrap();
        assert_eq!(result, json!(0));
    }
}
