//! `String.prototype.*` intrinsic implementations.
//!
//! Slice 10 introduced the table; task 30 finishes the foundation
//! method set. Every method dispatches through the
//! [`crate::intrinsics`] table so primitive string receivers reach
//! these implementations without allocating a wrapper object
//! (foundation plan rule #2).
//!
//! # Contents
//! - [`STRING_PROTOTYPE_TABLE`] — declarative table built with the
//!   [`crate::intrinsics!`] macro.
//! - One private `impl_*` function per method.
//!
//! # Invariants
//! - Every method validates the receiver as `Value::String`; a non-
//!   string raises [`crate::intrinsics::IntrinsicError::BadReceiver`].
//! - Numeric arguments accept `Value::Number` and (for foundation-era
//!   ergonomics on a few methods) string-encoded indices.
//! - `indexOf` polls the runtime interrupt flag every
//!   [`crate::string::INDEX_OF_INTERRUPT_BUDGET`] iterations.
//! - `toLowerCase` / `toUpperCase` are **ASCII-only**. Full Unicode
//!   case folding is deferred until ICU integration; non-ASCII code
//!   units pass through unchanged.
//! - `replace` / `replaceAll` perform **literal** substitution — the
//!   spec's `$&` / `$<n>` substitution patterns are not honoured.
//!   Regex-needle variants live in task 31.
//!
//! # See also
//! - [`docs/new-engine/tasks/10-string-methods-slice.md`](
//!     ../../../docs/new-engine/tasks/10-string-methods-slice.md
//!   )
//! - [`docs/new-engine/tasks/30-string-prototype-completion.md`](
//!     ../../../docs/new-engine/tasks/30-string-prototype-completion.md
//!   )

use crate::Value;
use crate::array::JsArray;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::regexp::JsRegExp;
use crate::string::Interrupted;
use crate::string::JsString;

fn receiver_string<'a>(args: &'a IntrinsicArgs<'_>) -> Result<&'a JsString, IntrinsicError> {
    match args.receiver {
        Value::String(s) => Ok(s),
        _ => Err(IntrinsicError::BadReceiver { expected: "string" }),
    }
}

fn arg_string<'a>(args: &'a IntrinsicArgs<'_>, index: u16) -> Result<&'a JsString, IntrinsicError> {
    match args.args.get(index as usize) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a string",
        }),
        None => Err(IntrinsicError::BadArgument {
            index,
            reason: "is required",
        }),
    }
}

/// Pull a u32 index from arg `index`. Accepts `Value::Number`
/// (clamped to `[0, u32::MAX]`) or, for foundation-era ergonomics,
/// `Value::String` whose body parses as a non-negative decimal
/// integer. Missing arguments collapse to `default`.
fn arg_u32_or(args: &IntrinsicArgs<'_>, index: u16, default: u32) -> Result<u32, IntrinsicError> {
    match args.args.get(index as usize) {
        None => Ok(default),
        Some(Value::Number(n)) => Ok(number_to_u32(*n)),
        Some(Value::String(s)) => parse_index(s).ok_or(IntrinsicError::BadArgument {
            index,
            reason: "must be a non-negative integer",
        }),
        Some(_) => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a non-negative integer",
        }),
    }
}

fn number_to_u32(n: NumberValue) -> u32 {
    match n.as_smi() {
        Some(v) if v >= 0 => v as u32,
        Some(_) => 0,
        None => {
            let f = n.as_f64();
            if f.is_nan() || f.is_sign_negative() {
                0
            } else if f >= u32::MAX as f64 {
                u32::MAX
            } else {
                f as u32
            }
        }
    }
}

fn parse_index(s: &JsString) -> Option<u32> {
    let text = s.to_lossy_string();
    text.trim().parse::<u32>().ok()
}

/// Pull a signed integer (negative-tolerant) from arg `index`.
/// Mirrors `ToIntegerOrInfinity` for the foundation subset:
/// `NaN`/missing/`undefined` → `default`; non-finite values clamp
/// to [`i64::MIN`] / [`i64::MAX`]; finite floats truncate toward
/// zero.
fn arg_int_or(args: &IntrinsicArgs<'_>, index: u16, default: i64) -> Result<i64, IntrinsicError> {
    match args.args.get(index as usize) {
        None | Some(Value::Undefined) => Ok(default),
        Some(Value::Number(n)) => Ok(number_to_int(*n)),
        Some(_) => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a number",
        }),
    }
}

fn number_to_int(n: NumberValue) -> i64 {
    if let Some(v) = n.as_smi() {
        return i64::from(v);
    }
    let f = n.as_f64();
    if f.is_nan() {
        0
    } else if f >= i64::MAX as f64 {
        i64::MAX
    } else if f <= i64::MIN as f64 {
        i64::MIN
    } else {
        f.trunc() as i64
    }
}

/// Spec `WhiteSpace` ∪ `LineTerminator` (§7.2 + §11.3).
///
/// USP characters from the Unicode `Space_Separator` category are
/// included via the explicit ranges below; the broader Unicode
/// space categories are deferred until ICU integration.
fn is_ws_code_unit(u: u16) -> bool {
    matches!(
        u,
        0x0009
            | 0x000A
            | 0x000B
            | 0x000C
            | 0x000D
            | 0x0020
            | 0x00A0
            | 0x1680
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
            | 0xFEFF
    ) || (0x2000..=0x200A).contains(&u)
}

/// One regex-match observation, normalised to plain code-unit
/// ranges. We collect all relevant matches up front so we can free
/// the iterator's borrow on the haystack before allocating
/// replacement strings.
#[derive(Debug, Clone)]
struct CapturedMatch {
    range: std::ops::Range<usize>,
    captures: Vec<Option<std::ops::Range<usize>>>,
}

/// Run a regex over `text_units` and collect every match. Honours
/// the `g` flag — without it we stop after the first match.
fn collect_regex_matches(re: &JsRegExp, text_units: &[u16]) -> Vec<CapturedMatch> {
    let mut out = Vec::new();
    for m in re.regex().find_from_utf16(text_units, 0) {
        out.push(CapturedMatch {
            range: m.range.clone(),
            captures: m.captures.clone(),
        });
        if !re.flags().global {
            break;
        }
    }
    out
}

/// `GetSubstitution`-lite: handles `$$`, `$&`, and `$1`–`$9`.
/// Named groups (`$<name>`) and `$'` / `$\`` are deferred.
fn apply_substitution(template: &[u16], text_units: &[u16], m: &CapturedMatch) -> Vec<u16> {
    let mut out = Vec::with_capacity(template.len());
    let mut i = 0;
    while i < template.len() {
        let c = template[i];
        if c != b'$' as u16 || i + 1 >= template.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let next = template[i + 1];
        match next {
            n if n == b'$' as u16 => {
                out.push(b'$' as u16);
                i += 2;
            }
            n if n == b'&' as u16 => {
                out.extend_from_slice(&text_units[m.range.clone()]);
                i += 2;
            }
            n if (b'1' as u16..=b'9' as u16).contains(&n) => {
                let idx = (n - b'0' as u16) as usize;
                if idx <= m.captures.len() {
                    if let Some(range) = &m.captures[idx - 1] {
                        out.extend_from_slice(&text_units[range.clone()]);
                    }
                } else {
                    // Out-of-range group → emit `$N` literally.
                    out.push(c);
                    out.push(next);
                }
                i += 2;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// First-occurrence search for `needle` inside `haystack` starting
/// at code-unit offset `from`. Used by methods that materialise
/// flat code-unit buffers (`replace`, `split`).
fn find_substr(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(haystack.len()));
    }
    if haystack.len() < needle.len() {
        return None;
    }
    let last_start = haystack.len() - needle.len();
    let mut i = from;
    while i <= last_start {
        if haystack[i..i + needle.len()] == needle[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn impl_length(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    Ok(Value::Number(NumberValue::from_i32(recv.len() as i32)))
}

fn impl_char_code_at(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let idx = arg_u32_or(args, 0, 0)?;
    let value = match recv.char_code_at(idx) {
        Some(unit) => NumberValue::from_i32(i32::from(unit)),
        None => NumberValue::Double(f64::NAN),
    };
    Ok(Value::Number(value))
}

fn impl_char_at(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let idx = arg_u32_or(args, 0, 0)?;
    let unit = recv.char_code_at(idx);
    match unit {
        Some(u) => {
            let s = JsString::from_utf16_units(&[u], args.string_heap)?;
            Ok(Value::String(s))
        }
        None => Ok(Value::String(JsString::empty(args.string_heap)?)),
    }
}

fn impl_slice(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let total = recv.len();
    let start = arg_u32_or(args, 0, 0)?.min(total);
    let end = match args.args.get(1) {
        Some(_) => arg_u32_or(args, 1, total)?.min(total),
        None => total,
    };
    let length = end.saturating_sub(start);
    let out = recv.slice(start, length, args.string_heap)?;
    Ok(Value::String(out))
}

fn impl_substring(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let total = recv.len();
    let mut start = arg_u32_or(args, 0, 0)?.min(total);
    let mut end = match args.args.get(1) {
        Some(_) => arg_u32_or(args, 1, total)?.min(total),
        None => total,
    };
    // Spec: if start > end, swap.
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    let length = end - start;
    let out = recv.slice(start, length, args.string_heap)?;
    Ok(Value::String(out))
}

fn impl_index_of(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_string(args, 0)?;
    let from = arg_u32_or(args, 1, 0)?;
    let pos =
        recv.index_of(needle, from, None)
            .map_err(|Interrupted| IntrinsicError::BadArgument {
                index: 0,
                reason: "interrupted",
            })?;
    let value = match pos {
        Some(p) => NumberValue::from_i32(p as i32),
        None => NumberValue::from_i32(-1),
    };
    Ok(Value::Number(value))
}

fn impl_starts_with(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_string(args, 0)?;
    let from = arg_u32_or(args, 1, 0)?;
    Ok(Value::Boolean(recv.starts_with(needle, from)))
}

fn impl_ends_with(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_string(args, 0)?;
    let end_pos = arg_u32_or(args, 1, recv.len())?;
    Ok(Value::Boolean(recv.ends_with(needle, end_pos)))
}

fn impl_includes(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_string(args, 0)?;
    let from = arg_u32_or(args, 1, 0)?;
    let pos =
        recv.index_of(needle, from, None)
            .map_err(|Interrupted| IntrinsicError::BadArgument {
                index: 0,
                reason: "interrupted",
            })?;
    Ok(Value::Boolean(pos.is_some()))
}

fn impl_concat(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let mut result = recv.clone();
    for (i, v) in args.args.iter().enumerate() {
        match v {
            Value::String(s) => {
                result = JsString::concat(&result, s, args.string_heap)?;
            }
            _ => {
                return Err(IntrinsicError::BadArgument {
                    index: i as u16,
                    reason: "must be a string",
                });
            }
        }
    }
    Ok(Value::String(result))
}

fn impl_repeat(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let count = arg_int_or(args, 0, 0)?;
    if count < 0 {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "must be non-negative",
        });
    }
    if count == 0 || recv.is_empty() {
        return Ok(Value::String(JsString::empty(args.string_heap)?));
    }
    let units = recv.to_utf16_vec();
    let total = (units.len() as u64).saturating_mul(count as u64);
    if total > u32::MAX as u64 {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "result would exceed maximum string length",
        });
    }
    let mut buf = Vec::with_capacity(total as usize);
    for _ in 0..count {
        buf.extend_from_slice(&units);
    }
    Ok(Value::String(JsString::from_utf16_units(
        &buf,
        args.string_heap,
    )?))
}

/// Pad-direction selector for [`pad_impl`].
#[derive(Clone, Copy)]
enum PadSide {
    Start,
    End,
}

fn pad_impl(args: &IntrinsicArgs<'_>, side: PadSide) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let target = arg_int_or(args, 0, 0)?;
    let recv_len = recv.len() as i64;
    if target <= recv_len {
        return Ok(Value::String(recv.clone()));
    }
    let pad_units: Vec<u16> = match args.args.get(1) {
        None | Some(Value::Undefined) => vec![0x0020],
        Some(Value::String(s)) => s.to_utf16_vec(),
        Some(_) => {
            return Err(IntrinsicError::BadArgument {
                index: 1,
                reason: "must be a string",
            });
        }
    };
    if pad_units.is_empty() {
        return Ok(Value::String(recv.clone()));
    }
    let pad_count = (target - recv_len) as usize;
    let recv_units = recv.to_utf16_vec();
    let mut buf: Vec<u16> = Vec::with_capacity(target as usize);
    let mut filled = 0;
    if matches!(side, PadSide::End) {
        buf.extend_from_slice(&recv_units);
    }
    while filled < pad_count {
        let take = (pad_count - filled).min(pad_units.len());
        buf.extend_from_slice(&pad_units[..take]);
        filled += take;
    }
    if matches!(side, PadSide::Start) {
        buf.extend_from_slice(&recv_units);
    }
    Ok(Value::String(JsString::from_utf16_units(
        &buf,
        args.string_heap,
    )?))
}

fn impl_pad_start(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    pad_impl(args, PadSide::Start)
}

fn impl_pad_end(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    pad_impl(args, PadSide::End)
}

/// Trim-direction selector for [`trim_impl`].
#[derive(Clone, Copy)]
enum TrimSide {
    Both,
    Start,
    End,
}

fn trim_impl(args: &IntrinsicArgs<'_>, side: TrimSide) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let units = recv.to_utf16_vec();
    let start = match side {
        TrimSide::Both | TrimSide::Start => units
            .iter()
            .position(|u| !is_ws_code_unit(*u))
            .unwrap_or(units.len()),
        TrimSide::End => 0,
    };
    let end = match side {
        TrimSide::Both | TrimSide::End => units
            .iter()
            .rposition(|u| !is_ws_code_unit(*u))
            .map_or(start, |i| i + 1),
        TrimSide::Start => units.len(),
    };
    let slice = if start <= end {
        &units[start..end]
    } else {
        &[][..]
    };
    Ok(Value::String(JsString::from_utf16_units(
        slice,
        args.string_heap,
    )?))
}

fn impl_trim(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    trim_impl(args, TrimSide::Both)
}

fn impl_trim_start(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    trim_impl(args, TrimSide::Start)
}

fn impl_trim_end(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    trim_impl(args, TrimSide::End)
}

fn impl_at(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let raw = arg_int_or(args, 0, 0)?;
    let len = recv.len() as i64;
    let idx = if raw < 0 {
        raw.saturating_add(len)
    } else {
        raw
    };
    if idx < 0 || idx >= len {
        return Ok(Value::Undefined);
    }
    let unit = recv
        .char_code_at(idx as u32)
        .expect("index in range yields a code unit");
    Ok(Value::String(JsString::from_utf16_units(
        &[unit],
        args.string_heap,
    )?))
}

fn impl_code_point_at(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let raw = arg_int_or(args, 0, 0)?;
    let len = recv.len() as i64;
    if raw < 0 || raw >= len {
        return Ok(Value::Undefined);
    }
    let idx = raw as u32;
    let cu1 = recv.char_code_at(idx).expect("index in range");
    if (0xD800..=0xDBFF).contains(&cu1) && (idx + 1) < len as u32 {
        let cu2 = recv.char_code_at(idx + 1).expect("idx+1 in range");
        if (0xDC00..=0xDFFF).contains(&cu2) {
            let cp = 0x10000u32 + ((u32::from(cu1) - 0xD800) << 10) + (u32::from(cu2) - 0xDC00);
            return Ok(Value::Number(NumberValue::from_i32(cp as i32)));
        }
    }
    Ok(Value::Number(NumberValue::from_i32(i32::from(cu1))))
}

fn map_ascii<F: Fn(u16) -> u16>(units: &[u16], f: F) -> Vec<u16> {
    units.iter().map(|&u| f(u)).collect()
}

fn impl_to_lower_case(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let units = recv.to_utf16_vec();
    let lowered = map_ascii(&units, |u| {
        if (u16::from(b'A')..=u16::from(b'Z')).contains(&u) {
            u + 32
        } else {
            u
        }
    });
    Ok(Value::String(JsString::from_utf16_units(
        &lowered,
        args.string_heap,
    )?))
}

fn impl_to_upper_case(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let units = recv.to_utf16_vec();
    let upper = map_ascii(&units, |u| {
        if (u16::from(b'a')..=u16::from(b'z')).contains(&u) {
            u - 32
        } else {
            u
        }
    });
    Ok(Value::String(JsString::from_utf16_units(
        &upper,
        args.string_heap,
    )?))
}

fn impl_replace(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    if let Some(Value::RegExp(re)) = args.args.first() {
        let replacement = arg_string(args, 1)?;
        return regex_replace(recv, re, &replacement.to_utf16_vec(), args.string_heap);
    }
    let needle = arg_string(args, 0)?;
    let replacement = arg_string(args, 1)?;
    let recv_units = recv.to_utf16_vec();
    let needle_units = needle.to_utf16_vec();
    let replacement_units = replacement.to_utf16_vec();

    if needle_units.is_empty() {
        let mut buf = Vec::with_capacity(recv_units.len() + replacement_units.len());
        buf.extend_from_slice(&replacement_units);
        buf.extend_from_slice(&recv_units);
        return Ok(Value::String(JsString::from_utf16_units(
            &buf,
            args.string_heap,
        )?));
    }
    let pos = match find_substr(&recv_units, &needle_units, 0) {
        Some(p) => p,
        None => return Ok(Value::String(recv.clone())),
    };
    let mut buf =
        Vec::with_capacity(recv_units.len() - needle_units.len() + replacement_units.len());
    buf.extend_from_slice(&recv_units[..pos]);
    buf.extend_from_slice(&replacement_units);
    buf.extend_from_slice(&recv_units[pos + needle_units.len()..]);
    Ok(Value::String(JsString::from_utf16_units(
        &buf,
        args.string_heap,
    )?))
}

fn impl_replace_all(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    if let Some(Value::RegExp(re)) = args.args.first() {
        // Spec: `replaceAll` requires the `g` flag for regex args.
        if !re.flags().global {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a global regular expression",
            });
        }
        let replacement = arg_string(args, 1)?;
        return regex_replace(recv, re, &replacement.to_utf16_vec(), args.string_heap);
    }
    let needle = arg_string(args, 0)?;
    let replacement = arg_string(args, 1)?;
    let recv_units = recv.to_utf16_vec();
    let needle_units = needle.to_utf16_vec();
    let replacement_units = replacement.to_utf16_vec();

    if needle_units.is_empty() {
        // Spec: insert replacement before each unit and at the end.
        let mut buf =
            Vec::with_capacity(recv_units.len() + replacement_units.len() * (recv_units.len() + 1));
        for &u in &recv_units {
            buf.extend_from_slice(&replacement_units);
            buf.push(u);
        }
        buf.extend_from_slice(&replacement_units);
        return Ok(Value::String(JsString::from_utf16_units(
            &buf,
            args.string_heap,
        )?));
    }
    if recv_units.len() < needle_units.len() {
        return Ok(Value::String(recv.clone()));
    }
    let last_start = recv_units.len() - needle_units.len();
    let mut buf = Vec::with_capacity(recv_units.len());
    let mut i = 0;
    while i <= last_start {
        if recv_units[i..i + needle_units.len()] == needle_units[..] {
            buf.extend_from_slice(&replacement_units);
            i += needle_units.len();
        } else {
            buf.push(recv_units[i]);
            i += 1;
        }
    }
    buf.extend_from_slice(&recv_units[i..]);
    Ok(Value::String(JsString::from_utf16_units(
        &buf,
        args.string_heap,
    )?))
}

fn impl_split(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;

    // Regex separator → defer to the dedicated walker.
    if let Some(Value::RegExp(re)) = args.args.first() {
        let limit = parse_split_limit(args)?;
        return regex_split(recv, re, limit, args.string_heap);
    }

    // Resolve separator: missing or `undefined` → caller-as-only-element.
    let separator = match args.args.first() {
        None | Some(Value::Undefined) => {
            return Ok(Value::Array(JsArray::from_elements([Value::String(
                recv.clone(),
            )])));
        }
        Some(Value::String(s)) => s,
        Some(_) => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a string",
            });
        }
    };

    let limit = parse_split_limit(args)?;
    if limit == 0 {
        return Ok(Value::Array(JsArray::new()));
    }

    let recv_units = recv.to_utf16_vec();
    let sep_units = separator.to_utf16_vec();

    // Empty separator: split into individual code units (capped).
    if sep_units.is_empty() {
        let mut out: Vec<Value> = Vec::with_capacity((limit as usize).min(recv_units.len()));
        for &u in recv_units.iter().take(limit as usize) {
            out.push(Value::String(JsString::from_utf16_units(
                &[u],
                args.string_heap,
            )?));
        }
        return Ok(Value::Array(JsArray::from_elements(out)));
    }

    let mut out: Vec<Value> = Vec::new();
    let mut start: usize = 0;
    while (out.len() as u32) < limit {
        match find_substr(&recv_units, &sep_units, start) {
            Some(pos) => {
                let part = JsString::from_utf16_units(&recv_units[start..pos], args.string_heap)?;
                out.push(Value::String(part));
                start = pos + sep_units.len();
            }
            None => break,
        }
    }
    if (out.len() as u32) < limit {
        let part = JsString::from_utf16_units(&recv_units[start..], args.string_heap)?;
        out.push(Value::String(part));
    }
    Ok(Value::Array(JsArray::from_elements(out)))
}

/// Common limit-arg parser shared by string-separator and
/// regex-separator `split` paths.
fn parse_split_limit(args: &IntrinsicArgs<'_>) -> Result<u32, IntrinsicError> {
    Ok(match args.args.get(1) {
        None | Some(Value::Undefined) => u32::MAX,
        Some(Value::Number(n)) => {
            let v = number_to_int(*n);
            if v < 0 {
                0
            } else if v > u32::MAX as i64 {
                u32::MAX
            } else {
                v as u32
            }
        }
        Some(_) => {
            return Err(IntrinsicError::BadArgument {
                index: 1,
                reason: "must be a number",
            });
        }
    })
}

fn regex_replace(
    recv: &JsString,
    re: &JsRegExp,
    replacement_template: &[u16],
    string_heap: &crate::string::StringHeap,
) -> Result<Value, IntrinsicError> {
    let recv_units = recv.to_utf16_vec();
    let matches = collect_regex_matches(re, &recv_units);
    if matches.is_empty() {
        return Ok(Value::String(recv.clone()));
    }
    let mut buf = Vec::with_capacity(recv_units.len());
    let mut cursor = 0;
    for m in &matches {
        buf.extend_from_slice(&recv_units[cursor..m.range.start]);
        let rendered = apply_substitution(replacement_template, &recv_units, m);
        buf.extend_from_slice(&rendered);
        cursor = m.range.end;
    }
    buf.extend_from_slice(&recv_units[cursor..]);
    Ok(Value::String(JsString::from_utf16_units(
        &buf,
        string_heap,
    )?))
}

fn regex_split(
    recv: &JsString,
    re: &JsRegExp,
    limit: u32,
    string_heap: &crate::string::StringHeap,
) -> Result<Value, IntrinsicError> {
    if limit == 0 {
        return Ok(Value::Array(JsArray::new()));
    }
    let recv_units = recv.to_utf16_vec();
    let mut out: Vec<Value> = Vec::new();
    let mut cursor: usize = 0;
    let mut iter = re.regex().find_from_utf16(&recv_units, 0);
    while (out.len() as u32) < limit {
        let m = match iter.next() {
            Some(m) => m,
            None => break,
        };
        // Spec quirk: zero-width match at the cursor is skipped to
        // prevent an infinite loop. We advance by one code unit and
        // try again.
        if m.range.start == cursor && m.range.end == cursor {
            if cursor >= recv_units.len() {
                break;
            }
            // Drop the iterator and resume after the cursor advance.
            drop(iter);
            cursor += 1;
            iter = re.regex().find_from_utf16(&recv_units, cursor);
            continue;
        }
        let part = JsString::from_utf16_units(&recv_units[cursor..m.range.start], string_heap)?;
        out.push(Value::String(part));
        cursor = m.range.end;
    }
    if (out.len() as u32) < limit {
        let part = JsString::from_utf16_units(&recv_units[cursor..], string_heap)?;
        out.push(Value::String(part));
    }
    Ok(Value::Array(JsArray::from_elements(out)))
}

fn impl_match(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let re = match args.args.first() {
        Some(Value::RegExp(r)) => r,
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a regular expression",
            });
        }
    };
    let recv_units = recv.to_utf16_vec();
    if re.flags().global {
        // `g` flag → return array of full matches only (no captures).
        let matches = collect_regex_matches(re, &recv_units);
        if matches.is_empty() {
            return Ok(Value::Null);
        }
        let mut out: Vec<Value> = Vec::with_capacity(matches.len());
        for m in &matches {
            let s = JsString::from_utf16_units(&recv_units[m.range.clone()], args.string_heap)?;
            out.push(Value::String(s));
        }
        return Ok(Value::Array(JsArray::from_elements(out)));
    }
    // Non-global → mirror `RegExp.prototype.exec`.
    let m = match re.regex().find_from_utf16(&recv_units, 0).next() {
        Some(m) => m,
        None => return Ok(Value::Null),
    };
    let mut out: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
    let full = JsString::from_utf16_units(&recv_units[m.range.clone()], args.string_heap)?;
    out.push(Value::String(full));
    for cap in &m.captures {
        match cap {
            Some(r) => {
                let s = JsString::from_utf16_units(&recv_units[r.clone()], args.string_heap)?;
                out.push(Value::String(s));
            }
            None => out.push(Value::Undefined),
        }
    }
    Ok(Value::Array(JsArray::from_elements(out)))
}

fn impl_match_all(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let re = match args.args.first() {
        Some(Value::RegExp(r)) => r,
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a regular expression",
            });
        }
    };
    if !re.flags().global {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "must be a global regular expression",
        });
    }
    let recv_units = recv.to_utf16_vec();
    let matches = collect_regex_matches(re, &recv_units);
    let mut out: Vec<Value> = Vec::with_capacity(matches.len());
    for m in &matches {
        let mut group: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
        let full = JsString::from_utf16_units(&recv_units[m.range.clone()], args.string_heap)?;
        group.push(Value::String(full));
        for cap in &m.captures {
            match cap {
                Some(r) => {
                    let s = JsString::from_utf16_units(&recv_units[r.clone()], args.string_heap)?;
                    group.push(Value::String(s));
                }
                None => group.push(Value::Undefined),
            }
        }
        out.push(Value::Array(JsArray::from_elements(group)));
    }
    Ok(Value::Array(JsArray::from_elements(out)))
}

fn impl_search(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let re = match args.args.first() {
        Some(Value::RegExp(r)) => r,
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a regular expression",
            });
        }
    };
    let recv_units = recv.to_utf16_vec();
    // `search` always starts at index 0 — `lastIndex` is ignored
    // and not mutated per spec §22.1.3.13.
    let pos = re
        .regex()
        .find_from_utf16(&recv_units, 0)
        .next()
        .map_or(-1, |m| m.range.start as i32);
    Ok(Value::Number(NumberValue::from_i32(pos)))
}

/// Declarative `String.prototype` table.
///
/// Task 30 brought the foundation-complete state for non-regex
/// methods; task 31 layers in the regex-arg overloads of `replace`
/// / `replaceAll` / `split` plus the new `match` / `matchAll` /
/// `search` entries.
pub static STRING_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            String,
            "length"        / 0 => impl_length,
            "charCodeAt"    / 1 => impl_char_code_at,
            "charAt"        / 1 => impl_char_at,
            "codePointAt"   / 1 => impl_code_point_at,
            "at"            / 1 => impl_at,
            "slice"         / 2 => impl_slice,
            "substring"     / 2 => impl_substring,
            "indexOf"       / 2 => impl_index_of,
            "includes"      / 2 => impl_includes,
            "startsWith"    / 2 => impl_starts_with,
            "endsWith"      / 2 => impl_ends_with,
            "concat"        / 1 => impl_concat,
            "repeat"        / 1 => impl_repeat,
            "padStart"      / 2 => impl_pad_start,
            "padEnd"        / 2 => impl_pad_end,
            "trim"          / 0 => impl_trim,
            "trimStart"     / 0 => impl_trim_start,
            "trimEnd"       / 0 => impl_trim_end,
            "toLowerCase"   / 0 => impl_to_lower_case,
            "toUpperCase"   / 0 => impl_to_upper_case,
            "replace"       / 2 => impl_replace,
            "replaceAll"    / 2 => impl_replace_all,
            "split"         / 2 => impl_split,
            "match"         / 1 => impl_match,
            "matchAll"      / 1 => impl_match_all,
            "search"        / 1 => impl_search,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    STRING_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::String, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::StringHeap;

    /// Drive an intrinsic with a string receiver. Argument inputs
    /// can be either decimal-integer strings (turned into
    /// `Value::Number`) or quoted forms — the helper auto-detects
    /// to keep the existing test cases readable.
    fn call(method: &str, recv: &str, args: &[&str]) -> String {
        let heap = StringHeap::default();
        let recv_v = Value::String(JsString::from_str(recv, &heap).unwrap());
        let arg_vs: Vec<Value> = args
            .iter()
            .map(|s| match s.parse::<i32>() {
                Ok(n) => Value::Number(NumberValue::from_i32(n)),
                Err(_) => Value::String(JsString::from_str(s, &heap).unwrap()),
            })
            .collect();
        let entry = lookup(method).unwrap();
        let result = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv_v,
            args: &arg_vs,
            string_heap: &heap,
        })
        .unwrap();
        result.display_string()
    }

    #[test]
    fn length() {
        assert_eq!(call("length", "abc", &[]), "3");
    }

    #[test]
    fn char_code_at_basic() {
        assert_eq!(call("charCodeAt", "abc", &["1"]), "98");
        assert_eq!(call("charCodeAt", "abc", &["10"]), "NaN");
    }

    #[test]
    fn char_at_basic() {
        assert_eq!(call("charAt", "abc", &["1"]), "b");
        assert_eq!(call("charAt", "abc", &["10"]), "");
    }

    #[test]
    fn slice_basic() {
        assert_eq!(call("slice", "abcdef", &["1", "4"]), "bcd");
        assert_eq!(call("slice", "abcdef", &["2"]), "cdef");
    }

    #[test]
    fn substring_swaps_when_reversed() {
        assert_eq!(call("substring", "abcdef", &["4", "1"]), "bcd");
    }

    #[test]
    fn index_of() {
        assert_eq!(call("indexOf", "abcabc", &["bc"]), "1");
        assert_eq!(call("indexOf", "abcabc", &["bc", "2"]), "4");
        assert_eq!(call("indexOf", "abcabc", &["zz"]), "-1");
    }

    #[test]
    fn starts_ends_with() {
        assert_eq!(call("startsWith", "hello", &["he"]), "true");
        assert_eq!(call("startsWith", "hello", &["lo"]), "false");
        assert_eq!(call("endsWith", "hello", &["lo"]), "true");
        assert_eq!(call("endsWith", "hello", &["he"]), "false");
    }

    #[test]
    fn bad_receiver_rejects() {
        let heap = StringHeap::default();
        let entry = lookup("length").unwrap();
        let err = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &Value::Undefined,
            args: &[],
            string_heap: &heap,
        })
        .unwrap_err();
        assert!(matches!(err, IntrinsicError::BadReceiver { .. }));
    }

    /// Argument shorthand for [`call_v`].
    enum A {
        N(i32),
        S(&'static str),
    }

    /// Drive an intrinsic with explicitly-typed arguments. Returns
    /// the raw [`Value`] so the caller can inspect non-string
    /// outputs (booleans, numbers, arrays).
    fn call_v(method: &str, recv: &str, args: &[A]) -> Value {
        let heap = StringHeap::default();
        let recv_v = Value::String(JsString::from_str(recv, &heap).unwrap());
        let arg_vs: Vec<Value> = args
            .iter()
            .map(|a| match a {
                A::N(n) => Value::Number(NumberValue::from_i32(*n)),
                A::S(s) => Value::String(JsString::from_str(s, &heap).unwrap()),
            })
            .collect();
        let entry = lookup(method).unwrap();
        (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv_v,
            args: &arg_vs,
            string_heap: &heap,
        })
        .unwrap()
    }

    fn call_s(method: &str, recv: &str, args: &[A]) -> String {
        call_v(method, recv, args).display_string()
    }

    #[test]
    fn includes_returns_boolean() {
        assert_eq!(
            call_v("includes", "abcabc", &[A::S("bc")]),
            Value::Boolean(true)
        );
        assert_eq!(
            call_v("includes", "abcabc", &[A::S("zz")]),
            Value::Boolean(false)
        );
        // includes uses `from` argument like indexOf.
        assert_eq!(
            call_v("includes", "abcabc", &[A::S("bc"), A::N(2)]),
            Value::Boolean(true)
        );
    }

    #[test]
    fn concat_joins_strings() {
        assert_eq!(call_s("concat", "ab", &[A::S("cd"), A::S("ef")]), "abcdef");
        assert_eq!(call_s("concat", "x", &[]), "x");
    }

    #[test]
    fn concat_rejects_non_string_args() {
        let heap = StringHeap::default();
        let recv = Value::String(JsString::from_str("a", &heap).unwrap());
        let entry = lookup("concat").unwrap();
        let err = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv,
            args: &[Value::Number(NumberValue::from_i32(3))],
            string_heap: &heap,
        })
        .unwrap_err();
        assert!(matches!(err, IntrinsicError::BadArgument { .. }));
    }

    #[test]
    fn repeat_basic() {
        assert_eq!(call_s("repeat", "abc", &[A::N(3)]), "abcabcabc");
        assert_eq!(call_s("repeat", "abc", &[A::N(0)]), "");
        assert_eq!(call_s("repeat", "", &[A::N(5)]), "");
    }

    #[test]
    fn repeat_rejects_negative() {
        let heap = StringHeap::default();
        let recv = Value::String(JsString::from_str("abc", &heap).unwrap());
        let entry = lookup("repeat").unwrap();
        let err = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv,
            args: &[Value::Number(NumberValue::from_i32(-1))],
            string_heap: &heap,
        })
        .unwrap_err();
        assert!(matches!(err, IntrinsicError::BadArgument { .. }));
    }

    #[test]
    fn pad_start_basic() {
        assert_eq!(call_s("padStart", "42", &[A::N(5), A::S("0")]), "00042");
        // Default pad is space.
        assert_eq!(call_s("padStart", "ab", &[A::N(5)]), "   ab");
        // Already long enough → original.
        assert_eq!(call_s("padStart", "hello", &[A::N(3), A::S("0")]), "hello");
        // Multi-char pad gets truncated to fit.
        assert_eq!(call_s("padStart", "x", &[A::N(5), A::S("ab")]), "ababx");
    }

    #[test]
    fn pad_end_basic() {
        assert_eq!(call_s("padEnd", "42", &[A::N(5), A::S("0")]), "42000");
        assert_eq!(call_s("padEnd", "ab", &[A::N(5)]), "ab   ");
        assert_eq!(call_s("padEnd", "x", &[A::N(5), A::S("ab")]), "xabab");
    }

    #[test]
    fn trim_methods() {
        assert_eq!(call_s("trim", "  hi  ", &[]), "hi");
        assert_eq!(call_s("trimStart", "  hi  ", &[]), "hi  ");
        assert_eq!(call_s("trimEnd", "  hi  ", &[]), "  hi");
        // Includes various whitespace chars.
        assert_eq!(call_s("trim", "\t\nhi\r\n", &[]), "hi");
        // All whitespace → empty.
        assert_eq!(call_s("trim", "   ", &[]), "");
    }

    #[test]
    fn at_basic() {
        assert_eq!(call_s("at", "abc", &[A::N(0)]), "a");
        assert_eq!(call_s("at", "abc", &[A::N(2)]), "c");
        assert_eq!(call_s("at", "abc", &[A::N(-1)]), "c");
        assert_eq!(call_s("at", "abc", &[A::N(-3)]), "a");
        // Out of range → undefined.
        assert_eq!(call_v("at", "abc", &[A::N(3)]), Value::Undefined);
        assert_eq!(call_v("at", "abc", &[A::N(-4)]), Value::Undefined);
    }

    #[test]
    fn code_point_at_basic() {
        // ASCII.
        assert_eq!(call_s("codePointAt", "abc", &[A::N(0)]), "97");
        // Out of range.
        assert_eq!(call_v("codePointAt", "abc", &[A::N(5)]), Value::Undefined);
    }

    #[test]
    fn code_point_at_combines_surrogates() {
        // U+10000 = '𐀀' = 0xD800 0xDC00
        let heap = StringHeap::default();
        let units: [u16; 3] = [0xD800, 0xDC00, b'a' as u16];
        let recv = Value::String(JsString::from_utf16_units(&units, &heap).unwrap());
        let entry = lookup("codePointAt").unwrap();
        let r = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv,
            args: &[Value::Number(NumberValue::from_i32(0))],
            string_heap: &heap,
        })
        .unwrap();
        assert_eq!(r.display_string(), "65536");
        // Index 1 is the trailing surrogate alone.
        let r2 = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv,
            args: &[Value::Number(NumberValue::from_i32(1))],
            string_heap: &heap,
        })
        .unwrap();
        assert_eq!(r2.display_string(), "56320");
    }

    #[test]
    fn case_methods_ascii_only() {
        assert_eq!(call_s("toLowerCase", "ABC", &[]), "abc");
        assert_eq!(call_s("toUpperCase", "abc", &[]), "ABC");
        // Mixed.
        assert_eq!(call_s("toLowerCase", "Hello, World!", &[]), "hello, world!");
        // Non-ASCII passes through unchanged.
        let heap = StringHeap::default();
        let units: [u16; 3] = [0x00C9, b'a' as u16, b'b' as u16]; // 'É' + "ab"
        let recv = Value::String(JsString::from_utf16_units(&units, &heap).unwrap());
        let entry = lookup("toLowerCase").unwrap();
        let r = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv,
            args: &[],
            string_heap: &heap,
        })
        .unwrap();
        // 'É' should stay (ASCII-only fold), 'a','b' lowercase.
        match r {
            Value::String(s) => {
                let v = s.to_utf16_vec();
                assert_eq!(v, vec![0x00C9, b'a' as u16, b'b' as u16]);
            }
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn replace_basic() {
        assert_eq!(
            call_s("replace", "abcabc", &[A::S("b"), A::S("X")]),
            "aXcabc"
        );
        // Empty needle → prepend.
        assert_eq!(call_s("replace", "abc", &[A::S(""), A::S("X")]), "Xabc");
        // No match → original.
        assert_eq!(call_s("replace", "abc", &[A::S("zz"), A::S("X")]), "abc");
    }

    #[test]
    fn replace_all_basic() {
        assert_eq!(
            call_s("replaceAll", "abcabc", &[A::S("b"), A::S("X")]),
            "aXcaXc"
        );
        // Empty needle: insert between every code unit and at ends.
        assert_eq!(
            call_s("replaceAll", "abc", &[A::S(""), A::S("X")]),
            "XaXbXcX"
        );
        // No match → original.
        assert_eq!(call_s("replaceAll", "abc", &[A::S("zz"), A::S("X")]), "abc");
        // Overlap-free advance.
        assert_eq!(call_s("replaceAll", "aaa", &[A::S("aa"), A::S("X")]), "Xa");
    }

    #[test]
    fn split_basic() {
        let v = call_v("split", "a,b,c", &[A::S(",")]);
        match v {
            Value::Array(a) => {
                assert_eq!(a.len(), 3);
                assert_eq!(a.get(0).display_string(), "a");
                assert_eq!(a.get(1).display_string(), "b");
                assert_eq!(a.get(2).display_string(), "c");
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_consecutive_separators_yield_empty_chunks() {
        let v = call_v("split", "a,,b", &[A::S(",")]);
        match v {
            Value::Array(a) => {
                assert_eq!(a.len(), 3);
                assert_eq!(a.get(0).display_string(), "a");
                assert_eq!(a.get(1).display_string(), "");
                assert_eq!(a.get(2).display_string(), "b");
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_empty_separator_yields_code_units() {
        let v = call_v("split", "abc", &[A::S("")]);
        match v {
            Value::Array(a) => {
                assert_eq!(a.len(), 3);
                assert_eq!(a.get(0).display_string(), "a");
                assert_eq!(a.get(1).display_string(), "b");
                assert_eq!(a.get(2).display_string(), "c");
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_with_limit() {
        let v = call_v("split", "a,b,c,d", &[A::S(","), A::N(2)]);
        match v {
            Value::Array(a) => {
                assert_eq!(a.len(), 2);
                assert_eq!(a.get(0).display_string(), "a");
                assert_eq!(a.get(1).display_string(), "b");
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_no_match_returns_singleton() {
        let v = call_v("split", "abc", &[A::S(",")]);
        match v {
            Value::Array(a) => {
                assert_eq!(a.len(), 1);
                assert_eq!(a.get(0).display_string(), "abc");
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_empty_receiver() {
        // "".split(",") === [""]
        let v = call_v("split", "", &[A::S(",")]);
        match v {
            Value::Array(a) => {
                assert_eq!(a.len(), 1);
                assert_eq!(a.get(0).display_string(), "");
            }
            _ => panic!("expected array"),
        }
        // "".split("") === []
        let v2 = call_v("split", "", &[A::S("")]);
        match v2 {
            Value::Array(a) => assert_eq!(a.len(), 0),
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_undefined_separator_returns_singleton() {
        // "abc".split() === ["abc"]
        let heap = StringHeap::default();
        let recv = Value::String(JsString::from_str("abc", &heap).unwrap());
        let entry = lookup("split").unwrap();
        let r = (entry.impl_fn)(&IntrinsicArgs {
            receiver: &recv,
            args: &[],
            string_heap: &heap,
        })
        .unwrap();
        match r {
            Value::Array(a) => {
                assert_eq!(a.len(), 1);
                assert_eq!(a.get(0).display_string(), "abc");
            }
            _ => panic!("expected array"),
        }
    }
}
