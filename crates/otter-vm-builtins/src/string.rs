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

use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get String ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // All ops now use native Value (no JSON conversion)
        // String.prototype methods
        op_native("__String_charAt", native_string_char_at),
        op_native("__String_charCodeAt", native_string_char_code_at),
        op_native("__String_codePointAt", native_string_code_point_at),
        op_native("__String_concat", native_string_concat),
        op_native("__String_includes", native_string_includes),
        op_native("__String_indexOf", native_string_index_of),
        op_native("__String_lastIndexOf", native_string_last_index_of),
        op_native("__String_slice", native_string_slice),
        op_native("__String_substring", native_string_substring),
        op_native("__String_split", native_string_split),
        op_native("__String_toLowerCase", native_string_to_lower_case),
        op_native("__String_toUpperCase", native_string_to_upper_case),
        op_native(
            "__String_toLocaleLowerCase",
            native_string_to_locale_lower_case,
        ),
        op_native(
            "__String_toLocaleUpperCase",
            native_string_to_locale_upper_case,
        ),
        op_native("__String_trim", native_string_trim),
        op_native("__String_trimStart", native_string_trim_start),
        op_native("__String_trimEnd", native_string_trim_end),
        op_native("__String_replace", native_string_replace),
        op_native("__String_replaceAll", native_string_replace_all),
        op_native("__String_startsWith", native_string_starts_with),
        op_native("__String_endsWith", native_string_ends_with),
        op_native("__String_repeat", native_string_repeat),
        op_native("__String_padStart", native_string_pad_start),
        op_native("__String_padEnd", native_string_pad_end),
        op_native("__String_length", native_string_length),
        op_native("__String_at", native_string_at),
        op_native("__String_normalize", native_string_normalize),
        op_native("__String_isWellFormed", native_string_is_well_formed),
        op_native("__String_toWellFormed", native_string_to_well_formed),
        op_native("__String_localeCompare", native_string_locale_compare),
        // Annex B - Legacy/deprecated methods
        op_native("__String_substr", native_string_substr),
        // HTML wrapper methods
        op_native("__String_anchor", native_string_anchor),
        op_native("__String_big", native_string_big),
        op_native("__String_blink", native_string_blink),
        op_native("__String_bold", native_string_bold),
        op_native("__String_fixed", native_string_fixed),
        op_native("__String_fontcolor", native_string_fontcolor),
        op_native("__String_fontsize", native_string_fontsize),
        op_native("__String_italics", native_string_italics),
        op_native("__String_link", native_string_link),
        op_native("__String_small", native_string_small),
        op_native("__String_strike", native_string_strike),
        op_native("__String_sub", native_string_sub),
        op_native("__String_sup", native_string_sup),
        // Static methods
        op_native("__String_fromCharCode", native_string_from_char_code),
        op_native("__String_fromCodePoint", native_string_from_code_point),
    ]
}

// =============================================================================
// Helper functions for native implementations
// =============================================================================

fn get_string(args: &[VmValue], index: usize) -> String {
    args.get(index)
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_default()
}

fn get_number(args: &[VmValue], index: usize) -> f64 {
    args.get(index).and_then(|v| v.as_number()).unwrap_or(0.0)
}

// =============================================================================
// String.prototype methods - Native implementations
// =============================================================================

fn native_string_char_at(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let index = get_number(args, 1) as usize;
    let ch = s
        .chars()
        .nth(index)
        .map(|c| c.to_string())
        .unwrap_or_default();
    Ok(VmValue::string(JsString::intern(&ch)))
}

fn native_string_char_code_at(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let index = get_number(args, 1) as usize;
    let utf16: Vec<u16> = s.encode_utf16().collect();
    if index < utf16.len() {
        Ok(VmValue::int32(utf16[index] as i32))
    } else {
        Ok(VmValue::nan())
    }
}

fn native_string_code_point_at(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let index = get_number(args, 1) as usize;
    match s.chars().nth(index) {
        Some(c) => Ok(VmValue::int32(c as i32)),
        None => Ok(VmValue::undefined()),
    }
}

fn native_string_concat(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let mut result = String::new();
    for arg in args {
        if let Some(s) = arg.as_string() {
            result.push_str(s.as_str());
        }
    }
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_includes(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let position = get_number(args, 2) as usize;

    if position >= s.chars().count() {
        return Ok(VmValue::boolean(false));
    }

    let substring: String = s.chars().skip(position).collect();
    Ok(VmValue::boolean(substring.contains(&search)))
}

fn native_string_index_of(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let position = get_number(args, 2) as usize;

    if position >= s.chars().count() {
        return Ok(VmValue::int32(-1));
    }

    let chars: Vec<char> = s.chars().collect();
    let search_chars: Vec<char> = search.chars().collect();

    if search_chars.is_empty() {
        return Ok(VmValue::int32(position as i32));
    }

    for i in position..=chars.len().saturating_sub(search_chars.len()) {
        if chars[i..i + search_chars.len()] == search_chars[..] {
            return Ok(VmValue::int32(i as i32));
        }
    }

    Ok(VmValue::int32(-1))
}

fn native_string_last_index_of(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let position = args.get(2).and_then(|v| v.as_number()).map(|n| n as usize);

    let chars: Vec<char> = s.chars().collect();
    let search_chars: Vec<char> = search.chars().collect();

    if search_chars.is_empty() {
        return Ok(VmValue::int32(chars.len() as i32));
    }

    let max_start = position
        .map(|p| p.min(chars.len().saturating_sub(search_chars.len())))
        .unwrap_or_else(|| chars.len().saturating_sub(search_chars.len()));

    for i in (0..=max_start).rev() {
        if i + search_chars.len() <= chars.len()
            && chars[i..i + search_chars.len()] == search_chars[..]
        {
            return Ok(VmValue::int32(i as i32));
        }
    }

    Ok(VmValue::int32(-1))
}

fn native_string_slice(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i32;

    let start = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let end = args.get(2).and_then(|v| v.as_number());

    let start = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;

    let end = match end {
        Some(e) => {
            let e = e as i32;
            if e < 0 {
                (len + e).max(0) as usize
            } else {
                (e.min(len)) as usize
            }
        }
        None => len as usize,
    };

    let result: String = chars
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_substring(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();

    let start = get_number(args, 1).max(0.0) as usize;
    let end = args
        .get(2)
        .and_then(|v| v.as_number())
        .map(|e| e.max(0.0) as usize)
        .unwrap_or(len);

    let (start, end) = (start.min(end).min(len), start.max(end).min(len));
    let result: String = chars.iter().skip(start).take(end - start).collect();
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_split(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let separator = args.get(1);
    let limit = args.get(2).and_then(|v| v.as_number()).map(|n| n as usize);

    if separator.is_none() || separator.map(|v| v.is_undefined()).unwrap_or(false) {
        let arr = GcRef::new(JsObject::array(1, Arc::clone(&mm)));
        let _ = arr.set(PropertyKey::Index(0), VmValue::string(JsString::intern(&s)));
        return Ok(VmValue::array(arr));
    }

    let sep = get_string(args, 1);
    let parts: Vec<String> = if sep.is_empty() {
        s.chars().map(|c| c.to_string()).collect()
    } else {
        s.split(&sep).map(|p| p.to_string()).collect()
    };

    let parts = match limit {
        Some(l) => parts.into_iter().take(l).collect::<Vec<_>>(),
        None => parts,
    };

    let arr = GcRef::new(JsObject::array(parts.len(), Arc::clone(&mm)));
    for (i, part) in parts.into_iter().enumerate() {
        let _ = arr.set(
            PropertyKey::Index(i as u32),
            VmValue::string(JsString::intern(&part)),
        );
    }
    Ok(VmValue::array(arr))
}

fn native_string_to_lower_case(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    Ok(VmValue::string(JsString::intern(&s.to_lowercase())))
}

fn native_string_to_upper_case(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    Ok(VmValue::string(JsString::intern(&s.to_uppercase())))
}

fn native_string_to_locale_lower_case(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    Ok(VmValue::string(JsString::intern(&s.to_lowercase())))
}

fn native_string_to_locale_upper_case(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    Ok(VmValue::string(JsString::intern(&s.to_uppercase())))
}

fn native_string_trim(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    Ok(VmValue::string(JsString::intern(s.trim())))
}

fn native_string_trim_start(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    Ok(VmValue::string(JsString::intern(s.trim_start())))
}

fn native_string_trim_end(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    Ok(VmValue::string(JsString::intern(s.trim_end())))
}

fn native_string_replace(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let replace = get_string(args, 2);

    let result = s.replacen(&search, &replace, 1);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_replace_all(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let replace = get_string(args, 2);

    let result = s.replace(&search, &replace);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_starts_with(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let position = get_number(args, 2) as usize;

    let substring: String = s.chars().skip(position).collect();
    Ok(VmValue::boolean(substring.starts_with(&search)))
}

fn native_string_ends_with(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let search = get_string(args, 1);
    let len = args
        .get(2)
        .and_then(|v| v.as_number())
        .map(|n| n as usize)
        .unwrap_or_else(|| s.chars().count());

    let substring: String = s.chars().take(len).collect();
    Ok(VmValue::boolean(substring.ends_with(&search)))
}

fn native_string_repeat(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let count = get_number(args, 1) as usize;

    if count == 0 {
        return Ok(VmValue::string(JsString::intern("")));
    }

    let result = s.repeat(count);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_pad_start(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let target_len = get_number(args, 1) as usize;
    let pad_str = args
        .get(2)
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| " ".to_string());

    if target_len <= s.len() || pad_str.is_empty() {
        return Ok(VmValue::string(JsString::intern(&s)));
    }

    let pad_len = target_len - s.len();
    let mut result = String::new();
    while result.len() < pad_len {
        result.push_str(&pad_str);
    }
    result.truncate(pad_len);
    result.push_str(&s);

    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_pad_end(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let target_len = get_number(args, 1) as usize;
    let pad_str = args
        .get(2)
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| " ".to_string());

    if target_len <= s.len() || pad_str.is_empty() {
        return Ok(VmValue::string(JsString::intern(&s)));
    }

    let _pad_len = target_len - s.len();
    let mut result = s.clone();
    while result.len() < target_len {
        result.push_str(&pad_str);
    }
    result.truncate(target_len);

    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_length(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let utf16_len = s.encode_utf16().count();
    Ok(VmValue::int32(utf16_len as i32))
}

fn native_string_at(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let chars: Vec<char> = s.chars().collect();
    let index = get_number(args, 1) as i32;

    let actual_index = if index < 0 {
        (chars.len() as i32 + index) as usize
    } else {
        index as usize
    };

    match chars.get(actual_index) {
        Some(&ch) => Ok(VmValue::string(JsString::intern(&ch.to_string()))),
        None => Ok(VmValue::undefined()),
    }
}

fn native_string_normalize(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    // Simplified: just return the string as-is (full Unicode normalization is complex)
    Ok(VmValue::string(JsString::intern(&s)))
}

fn native_string_is_well_formed(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    // Check if string contains unpaired surrogates
    let utf16: Vec<u16> = s.encode_utf16().collect();
    let mut i = 0;
    while i < utf16.len() {
        let code = utf16[i];
        if (0xD800..=0xDBFF).contains(&code) {
            // High surrogate
            if i + 1 >= utf16.len() || !(0xDC00..=0xDFFF).contains(&utf16[i + 1]) {
                return Ok(VmValue::boolean(false));
            }
            i += 2;
        } else if (0xDC00..=0xDFFF).contains(&code) {
            // Unpaired low surrogate
            return Ok(VmValue::boolean(false));
        } else {
            i += 1;
        }
    }
    Ok(VmValue::boolean(true))
}

fn native_string_to_well_formed(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    // Replace unpaired surrogates with U+FFFD
    let utf16: Vec<u16> = s.encode_utf16().collect();
    let mut result = String::new();
    let mut i = 0;
    while i < utf16.len() {
        let code = utf16[i];
        if (0xD800..=0xDBFF).contains(&code) {
            if i + 1 < utf16.len() && (0xDC00..=0xDFFF).contains(&utf16[i + 1]) {
                // Valid surrogate pair
                if let Some(ch) = char::decode_utf16([code, utf16[i + 1]].iter().copied()).next() {
                    if let Ok(ch) = ch {
                        result.push(ch);
                    } else {
                        result.push('\u{FFFD}');
                    }
                }
                i += 2;
            } else {
                // Unpaired high surrogate
                result.push('\u{FFFD}');
                i += 1;
            }
        } else if (0xDC00..=0xDFFF).contains(&code) {
            // Unpaired low surrogate
            result.push('\u{FFFD}');
            i += 1;
        } else {
            if let Some(ch) = char::decode_utf16([code].iter().copied()).next() {
                if let Ok(ch) = ch {
                    result.push(ch);
                }
            }
            i += 1;
        }
    }
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_locale_compare(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let s1 = get_string(args, 0);
    let s2 = get_string(args, 1);

    let result = s1.cmp(&s2);
    Ok(VmValue::int32(match result {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }))
}

fn native_string_substr(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let chars: Vec<char> = s.chars().collect();
    let start = get_number(args, 1) as i32;
    let length = args.get(2).and_then(|v| v.as_number()).map(|n| n as usize);

    let start = if start < 0 {
        (chars.len() as i32 + start).max(0) as usize
    } else {
        (start as usize).min(chars.len())
    };

    let length = length
        .unwrap_or(chars.len() - start)
        .min(chars.len() - start);
    let result: String = chars.iter().skip(start).take(length).collect();
    Ok(VmValue::string(JsString::intern(&result)))
}

// HTML wrapper methods - all deprecated but required for spec compliance
fn native_string_anchor(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let name = get_string(args, 1);
    let result = format!("<a name=\"{}\">{}</a>", name, s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_big(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let result = format!("<big>{}</big>", s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_blink(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let result = format!("<blink>{}</blink>", s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_bold(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let result = format!("<b>{}</b>", s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_fixed(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let result = format!("<tt>{}</tt>", s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_fontcolor(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let color = get_string(args, 1);
    let result = format!("<font color=\"{}\">{}</font>", color, s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_fontsize(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let size = get_string(args, 1);
    let result = format!("<font size=\"{}\">{}</font>", size, s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_italics(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let result = format!("<i>{}</i>", s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_link(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let url = get_string(args, 1);
    let result = format!("<a href=\"{}\">{}</a>", url, s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_small(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let result = format!("<small>{}</small>", s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_strike(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let result = format!("<strike>{}</strike>", s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_sub(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let result = format!("<sub>{}</sub>", s);
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_sup(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let s = get_string(args, 0);
    let result = format!("<sup>{}</sup>", s);
    Ok(VmValue::string(JsString::intern(&result)))
}

// Static methods
fn native_string_from_char_code(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let mut result = String::new();
    for arg in args {
        if let Some(n) = arg.as_number() {
            let code = (n as u32) & 0xFFFF;
            if let Some(ch) = char::from_u32(code) {
                result.push(ch);
            }
        }
    }
    Ok(VmValue::string(JsString::intern(&result)))
}

fn native_string_from_code_point(
    args: &[VmValue],
    _mm: Arc<MemoryManager>,
) -> Result<VmValue, VmError> {
    let mut result = String::new();
    for arg in args {
        if let Some(n) = arg.as_number() {
            let code = n as u32;
            if let Some(ch) = char::from_u32(code) {
                result.push(ch);
            } else {
                return Err(VmError::range_error(format!(
                    "Invalid code point: {}",
                    code
                )));
            }
        }
    }
    Ok(VmValue::string(JsString::intern(&result)))
}
