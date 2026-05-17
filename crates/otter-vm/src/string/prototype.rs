//! `String.prototype.*` intrinsic implementations.
//!
//! Every method dispatches through the
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
//! - Every method validates the receiver as a primitive string or a
//!   String wrapper with `[[StringData]]`; a non-string raises
//!   [`crate::intrinsics::IntrinsicError::BadReceiver`].
//! - Numeric arguments accept `Value::Number` and (for foundation-era
//!   ergonomics on a few methods) string-encoded indices.
//! - `indexOf` polls the runtime interrupt flag every
//!   [`crate::string::INDEX_OF_INTERRUPT_BUDGET`] iterations.
//! - `toLowerCase` / `toUpperCase` are **ASCII-only**. Full Unicode
//!   case folding is deferred until ICU integration; non-ASCII code
//!   units pass through unchanged.
//! - `replace` / `replaceAll` perform **literal** substitution — the
//!   spec's `$&` / `$<n>` substitution patterns are not honoured.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-string-prototype-object>

use smallvec::SmallVec;

use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::number::NumberValue;
use crate::regexp::JsRegExp;
use crate::string::Interrupted;
use crate::string::JsString;
use crate::{NativeCtx, NativeError};

/// §22.1.3.1 thisStringValue / §7.1.17 ToString glue for
/// `String.prototype.*` receivers.
///
/// Spec algorithm per method:
/// 1. `RequireObjectCoercible(O)` — `null` / `undefined` reject with
///    TypeError.
/// 2. `S = ? ToString(O)` — primitives coerce via §7.1.17, wrapper
///    objects read `[[StringData]]`, plain objects walk the
///    `Symbol.toPrimitive` / `toString` / `valueOf` ladder (not yet
///    wired here — see callers of `receiver_string`).
///
/// We accept:
/// - `Value::String` directly.
/// - `Value::Object` carrying `[[StringData]]` (String wrapper).
/// - `Value::Boolean` → `"true"` / `"false"`.
/// - `Value::Number` → display-string form.
/// - `Value::BigInt` → its decimal display.
/// - `Value::Symbol` rejects (§22.1.3.7 — Symbol receivers throw
///   TypeError in every String.prototype.* method).
/// - `Value::Null` / `Value::Undefined` reject per
///   `RequireObjectCoercible`.
fn receiver_string(args: &IntrinsicArgs<'_>) -> Result<JsString, IntrinsicError> {
    match args.receiver {
        Value::String(s) => Ok(s.clone()),
        Value::Object(obj) => {
            let gc = &*args.gc_heap;
            // Wrapper objects expose their primitive via the matching
            // internal slot; missing-slot ordinary objects fall back
            // to `Object.prototype.toString`'s "[object Object]"
            // shape so `String.prototype.*.call({})` doesn't bail.
            // User-defined `toString` overrides require an
            // `ExecutionContext` we don't carry into the intrinsic
            // layer yet — handled in a follow-up.
            if let Some(s) = crate::object::string_data(*obj, gc) {
                return Ok(s);
            }
            if let Some(b) = crate::object::boolean_data(*obj, gc) {
                let text = if b { "true" } else { "false" };
                return Ok(JsString::from_str(text, args.string_heap)?);
            }
            if let Some(n) = crate::object::number_data(*obj, gc) {
                let text = n.to_display_string();
                return Ok(JsString::from_str(&text, args.string_heap)?);
            }
            Ok(JsString::from_str("[object Object]", args.string_heap)?)
        }
        Value::Boolean(b) => {
            let text = if *b { "true" } else { "false" };
            Ok(JsString::from_str(text, args.string_heap)?)
        }
        Value::Number(n) => {
            let text = n.to_display_string();
            Ok(JsString::from_str(&text, args.string_heap)?)
        }
        Value::BigInt(b) => {
            let text = b.to_decimal_string();
            Ok(JsString::from_str(&text, args.string_heap)?)
        }
        Value::Array(arr) => {
            // §22.1.3.32 Array.prototype.toString → Array.prototype.join(",").
            // We mirror the spec result for the common no-override
            // case: comma-joined dense elements with null/undefined
            // displayed as "".
            let gc = &*args.gc_heap;
            let items: Vec<String> = crate::array::with_elements(*arr, gc, |els| {
                els.iter()
                    .map(|v| match v {
                        Value::Null | Value::Undefined | Value::Hole => String::new(),
                        Value::String(s) => s.to_lossy_string(),
                        Value::Number(n) => n.to_display_string(),
                        Value::Boolean(b) => {
                            if *b {
                                "true".to_string()
                            } else {
                                "false".to_string()
                            }
                        }
                        _ => v.display_string(),
                    })
                    .collect()
            });
            Ok(JsString::from_str(&items.join(","), args.string_heap)?)
        }
        Value::RegExp(re) => {
            // §22.2.6.13 RegExp.prototype.toString — `/source/flags`.
            let gc = &*args.gc_heap;
            let pattern = re.source(gc);
            let flags = re.flags(gc);
            let pattern_str = if pattern.is_empty() {
                "(?:)".to_string()
            } else {
                pattern
            };
            let text = format!("/{}/{}", pattern_str, flags.to_js_string());
            Ok(JsString::from_str(&text, args.string_heap)?)
        }
        Value::Symbol(_) => Err(IntrinsicError::BadReceiver { expected: "string" }),
        Value::Null | Value::Undefined => Err(IntrinsicError::BadReceiver { expected: "string" }),
        _ => Err(IntrinsicError::BadReceiver { expected: "string" }),
    }
}

/// §7.1.17 ToString applied to argument `index`.
///
/// Returns an owned `JsString`. Primitives coerce directly; wrapper
/// objects with `[[StringData]]` unwrap; `Symbol` and ordinary
/// objects bail (the latter needs §7.1.1 OrdinaryToPrimitive routing
/// through user-defined `toString`, which sits behind an
/// `ExecutionContext` we don't carry here yet).
///
/// Missing arguments coerce to `"undefined"` per §7.1.17 step 1
/// (`ToString(undefined) = "undefined"`).
fn arg_to_string(args: &IntrinsicArgs<'_>, index: u16) -> Result<JsString, IntrinsicError> {
    match args.args.get(index as usize) {
        None | Some(Value::Undefined) => Ok(JsString::from_str("undefined", args.string_heap)?),
        Some(Value::Null) => Ok(JsString::from_str("null", args.string_heap)?),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Boolean(b)) => {
            let text = if *b { "true" } else { "false" };
            Ok(JsString::from_str(text, args.string_heap)?)
        }
        Some(Value::Number(n)) => {
            let text = n.to_display_string();
            Ok(JsString::from_str(&text, args.string_heap)?)
        }
        Some(Value::BigInt(b)) => {
            let text = b.to_decimal_string();
            Ok(JsString::from_str(&text, args.string_heap)?)
        }
        Some(Value::Object(obj)) => {
            // Wrapper objects with `[[StringData]]` unwrap directly.
            // Plain objects routing through §7.1.1 OrdinaryToPrimitive
            // is not yet wired here; we still reject for those.
            let gc = &*args.gc_heap;
            crate::object::string_data(*obj, gc).ok_or(IntrinsicError::BadArgument {
                index,
                reason: "must be a string",
            })
        }
        Some(Value::Symbol(_)) => Err(IntrinsicError::BadArgument {
            index,
            reason: "Symbol values cannot be converted to a string",
        }),
        Some(_) => Err(IntrinsicError::BadArgument {
            index,
            reason: "must be a string",
        }),
    }
}

/// Pull a u32 index from arg `index`. §7.1.5 ToUint32 coerces every
/// spec-relevant operand: `Value::Number` clamps to `[0, u32::MAX]`,
/// `Value::Boolean` (true → 1, false → 0), `Value::Null` → 0,
/// `Value::String` parses as decimal (NaN-on-failure clamps to 0
/// per ToUint32 modulo), `Value::Undefined` and missing arguments
/// collapse to `default`.
fn arg_u32_or(args: &IntrinsicArgs<'_>, index: u16, default: u32) -> Result<u32, IntrinsicError> {
    match args.args.get(index as usize) {
        None | Some(Value::Undefined) => Ok(default),
        Some(Value::Number(n)) => Ok(number_to_u32(*n)),
        Some(Value::Boolean(true)) => Ok(1),
        Some(Value::Boolean(false)) | Some(Value::Null) => Ok(0),
        Some(Value::String(s)) => Ok(parse_index(s).unwrap_or(0)),
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
    // §7.1.5 ToIntegerOrInfinity — coerce the spec-relevant operand
    // set (Number / Boolean / Null / String) before treating
    // non-finite / NaN as the default.
    match args.args.get(index as usize) {
        None | Some(Value::Undefined) => Ok(default),
        Some(Value::Number(n)) => Ok(number_to_int(*n)),
        Some(Value::Boolean(true)) => Ok(1),
        Some(Value::Boolean(false)) | Some(Value::Null) => Ok(0),
        Some(Value::String(s)) => {
            let text = s.to_lossy_string();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(0)
            } else {
                Ok(trimmed.parse::<i64>().unwrap_or(0))
            }
        }
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

/// Run a regex over `text_units` and collect every match. Honours
/// the `g` flag — without it we stop after the first match. We
/// collect `regress::Match` directly: it is already owned (owned
/// `Range<usize>` ranges, owned capture vec, owned group-name table)
/// so we can release the iterator's borrow on `text_units` before
/// allocating replacement strings or building result arrays.
fn collect_regex_matches(
    re: &JsRegExp,
    gc_heap: &otter_gc::GcHeap,
    text_units: &[u16],
) -> Vec<regress::Match> {
    let mut out = Vec::new();
    for m in re.find_from_utf16(gc_heap, text_units, 0) {
        out.push(m);
        if !re.flags(gc_heap).global {
            break;
        }
    }
    out
}

/// `GetSubstitution`-lite: handles `$$`, `$&`, and `$1`–`$9`.
/// Named groups (`$<name>`) and `$'` / `$\`` are deferred.
fn apply_substitution(template: &[u16], text_units: &[u16], m: &regress::Match) -> Vec<u16> {
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
///
/// SWAR-assisted: the candidate scan for the needle's first code
/// unit goes through [`crate::swar::find_u16`] (4 lanes per
/// `u64`); the verify step is a single slice equality.
fn find_substr(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(haystack.len()));
    }
    if haystack.len() < needle.len() {
        return None;
    }
    let last_start = haystack.len() - needle.len();
    let first = needle[0];
    let n_len = needle.len();
    let mut search_start = from;
    while search_start <= last_start {
        let rel = crate::swar::find_u16(&haystack[search_start..=last_start], first, 0)?;
        let i = search_start + rel;
        if haystack[i..i + n_len] == *needle {
            return Some(i);
        }
        search_start = i + 1;
    }
    None
}

fn impl_length(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    Ok(Value::Number(NumberValue::from_i32(recv.len() as i32)))
}

fn impl_char_code_at(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let idx = arg_u32_or(args, 0, 0)?;
    let value = match recv.char_code_at(idx) {
        Some(unit) => NumberValue::from_i32(i32::from(unit)),
        None => NumberValue::Double(f64::NAN),
    };
    Ok(Value::Number(value))
}

fn impl_char_at(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
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

fn impl_slice(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
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

fn impl_substring(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
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

fn impl_index_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_to_string(args, 0)?;
    let from = arg_u32_or(args, 1, 0)?;
    let pos =
        recv.index_of(&needle, from, None)
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

fn impl_starts_with(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_to_string(args, 0)?;
    let from = arg_u32_or(args, 1, 0)?;
    Ok(Value::Boolean(recv.starts_with(&needle, from)))
}

fn impl_ends_with(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_to_string(args, 0)?;
    let end_pos = arg_u32_or(args, 1, recv.len())?;
    Ok(Value::Boolean(recv.ends_with(&needle, end_pos)))
}

fn impl_includes(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_to_string(args, 0)?;
    let from = arg_u32_or(args, 1, 0)?;
    let pos =
        recv.index_of(&needle, from, None)
            .map_err(|Interrupted| IntrinsicError::BadArgument {
                index: 0,
                reason: "interrupted",
            })?;
    Ok(Value::Boolean(pos.is_some()))
}

fn impl_concat(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    // §22.1.3.5 — `for each next of arguments: nextString = ?
    // ToString(next)`. Coerce every operand via the shared
    // `arg_to_string` helper (primitives + wrapper objects with
    // `[[StringData]]`); plain objects without an inherited
    // `toString` still reject.
    let recv = receiver_string(args)?;
    let mut result = recv.clone();
    for i in 0..args.args.len() {
        let piece = arg_to_string(args, i as u16)?;
        result = JsString::concat(&result, &piece, args.string_heap)?;
    }
    Ok(Value::String(result))
}

fn impl_repeat(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
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

fn impl_pad_start(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    pad_impl(args, PadSide::Start)
}

fn impl_pad_end(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
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

fn impl_trim(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    trim_impl(args, TrimSide::Both)
}

/// §B.2.3.1 `CreateHTML(string, tag, attribute, value)`.
///
/// Wraps the receiver string in an HTML tag, optionally with a
/// single `attribute="value"` pair (double-quoted, `"` in the value
/// escaped to `&quot;`). Implements the foundation of the
/// `String.prototype.{anchor, big, blink, bold, fixed, fontcolor,
/// fontsize, italics, link, small, strike, sub, sup}` AnnexB shims.
fn create_html(
    args: &mut IntrinsicArgs<'_>,
    tag: &str,
    attribute: Option<&str>,
) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let body = recv.to_lossy_string();
    let mut out = String::with_capacity(body.len() + tag.len() * 2 + 5);
    out.push('<');
    out.push_str(tag);
    if let Some(attr) = attribute {
        let raw = match args.args.first() {
            None | Some(Value::Undefined) => "undefined".to_string(),
            Some(Value::String(s)) => s.to_lossy_string(),
            Some(other) => other.display_string(),
        };
        let escaped = raw.replace('"', "&quot;");
        out.push(' ');
        out.push_str(attr);
        out.push('=');
        out.push('"');
        out.push_str(&escaped);
        out.push('"');
    }
    out.push('>');
    out.push_str(&body);
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
    Ok(Value::String(JsString::from_str(&out, args.string_heap)?))
}

/// §B.2.3.1 `String.prototype.substr(start, length)`.
///
/// 1. Let `O` be `? RequireObjectCoercible(this)`.
/// 2. Let `S` be `? ToString(O)`.
/// 3. Let `size` be the length of `S`.
/// 4. Let `intStart` be `? ToIntegerOrInfinity(start)`. If `-∞`,
///    clamp to 0; if negative, clamp to `max(size + intStart, 0)`;
///    else clamp to `min(intStart, size)`.
/// 5. If `length` is undefined → `intLength = size`; else
///    `intLength = ? ToIntegerOrInfinity(length)` and clamp to
///    `min(max(intLength, 0), size - intStart)`.
/// 6. If `intLength <= 0` return the empty string.
/// 7. Return the substring of `S` from `intStart` of length `intLength`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-string.prototype.substr>

/// §22.1.3.10 `String.prototype.isWellFormed()`. Returns `true` if
/// every surrogate code unit is part of a valid pair.
fn impl_is_well_formed(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let units = recv.to_utf16_vec();
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if (0xD800..=0xDBFF).contains(&u) {
            if i + 1 >= units.len() || !(0xDC00..=0xDFFF).contains(&units[i + 1]) {
                return Ok(Value::Boolean(false));
            }
            i += 2;
        } else if (0xDC00..=0xDFFF).contains(&u) {
            return Ok(Value::Boolean(false));
        } else {
            i += 1;
        }
    }
    Ok(Value::Boolean(true))
}

/// §22.1.3.11 `String.prototype.toWellFormed()`. Replaces every
/// unpaired surrogate with `U+FFFD` (REPLACEMENT CHARACTER).
fn impl_to_well_formed(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let units = recv.to_utf16_vec();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if (0xD800..=0xDBFF).contains(&u) {
            if i + 1 < units.len() && (0xDC00..=0xDFFF).contains(&units[i + 1]) {
                out.push(u);
                out.push(units[i + 1]);
                i += 2;
                continue;
            }
            out.push(0xFFFD);
        } else if (0xDC00..=0xDFFF).contains(&u) {
            out.push(0xFFFD);
        } else {
            out.push(u);
        }
        i += 1;
    }
    Ok(Value::String(JsString::from_utf16_units(
        &out,
        args.string_heap,
    )?))
}

fn impl_substr(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let size = recv.len() as i64;
    let raw_start = arg_int_or(args, 0, 0)?;
    let int_start = if raw_start == i64::MIN {
        0
    } else if raw_start < 0 {
        std::cmp::max(size + raw_start, 0)
    } else {
        std::cmp::min(raw_start, size)
    };
    let int_length = match args.args.get(1) {
        None | Some(Value::Undefined) => size,
        Some(_) => {
            let raw = arg_int_or(args, 1, 0)?;
            std::cmp::min(std::cmp::max(raw, 0), size - int_start)
        }
    };
    if int_length <= 0 {
        return Ok(Value::String(JsString::empty(args.string_heap)?));
    }
    Ok(Value::String(recv.slice(
        int_start as u32,
        int_length as u32,
        args.string_heap,
    )?))
}

fn impl_anchor(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "a", Some("name"))
}
fn impl_big(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "big", None)
}
fn impl_blink(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "blink", None)
}
fn impl_bold(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "b", None)
}
fn impl_fixed(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "tt", None)
}
fn impl_fontcolor(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "font", Some("color"))
}
fn impl_fontsize(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "font", Some("size"))
}
fn impl_italics(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "i", None)
}
fn impl_link(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "a", Some("href"))
}
fn impl_small(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "small", None)
}
fn impl_strike(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "strike", None)
}
fn impl_sub(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "sub", None)
}
fn impl_sup(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    create_html(args, "sup", None)
}

fn impl_trim_start(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    trim_impl(args, TrimSide::Start)
}

fn impl_trim_end(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    trim_impl(args, TrimSide::End)
}

fn impl_at(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
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

fn impl_code_point_at(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
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

fn impl_to_lower_case(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
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

fn impl_to_upper_case(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
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

fn impl_replace(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    if let Some(Value::RegExp(re)) = args.args.first() {
        let replacement = arg_to_string(args, 1)?;
        let heap = &*args.gc_heap;
        return regex_replace(
            &recv,
            re,
            heap,
            &replacement.to_utf16_vec(),
            args.string_heap,
        );
    }
    let needle = arg_to_string(args, 0)?;
    let replacement = arg_to_string(args, 1)?;
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

fn impl_replace_all(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    if let Some(Value::RegExp(re)) = args.args.first() {
        // Spec: `replaceAll` requires the `g` flag for regex args.
        let heap = &*args.gc_heap;
        if !re.flags(heap).global {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a global regular expression",
            });
        }
        let replacement = arg_to_string(args, 1)?;
        return regex_replace(
            &recv,
            re,
            heap,
            &replacement.to_utf16_vec(),
            args.string_heap,
        );
    }
    let needle = arg_to_string(args, 0)?;
    let replacement = arg_to_string(args, 1)?;
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

fn impl_split(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;

    // Regex separator → defer to the dedicated walker.
    if let Some(Value::RegExp(re)) = args.args.first() {
        let re = *re;
        let limit = parse_split_limit(args)?;
        return regex_split(&recv, &re, limit, args);
    }

    // Resolve separator: missing or `undefined` → caller-as-only-element.
    // §7.1.17 ToString coerces every other operand (Boolean / Number /
    // BigInt / Null / wrapper objects) before the search.
    let separator_owned: JsString;
    let separator = match args.args.first() {
        None | Some(Value::Undefined) => {
            let singleton = [Value::String(recv.clone())];
            return Ok(Value::Array(args.array_from_elements_rooted(
                singleton.iter().cloned(),
                &[],
                &[singleton.as_slice()],
            )?));
        }
        Some(Value::String(s)) => s,
        Some(_) => {
            separator_owned = arg_to_string(args, 0)?;
            &separator_owned
        }
    };

    let limit = parse_split_limit(args)?;
    if limit == 0 {
        return Ok(Value::Array(args.array_from_elements_rooted(
            std::iter::empty(),
            &[],
            &[],
        )?));
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
        return Ok(Value::Array(args.array_from_elements_rooted(
            out.iter().cloned(),
            &[],
            &[out.as_slice()],
        )?));
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
    Ok(Value::Array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// Common limit-arg parser shared by string-separator and
/// regex-separator `split` paths.
fn parse_split_limit(args: &IntrinsicArgs<'_>) -> Result<u32, IntrinsicError> {
    // §22.1.3.23 step 6: `limit` defaults to 2^32 - 1 and is
    // ToUint32-coerced. Foundation accepts the spec set
    // (`Number` / `Boolean` / `null` / `String` — strings parsed as
    // decimal integers). Non-integer / negative coerce to 0 per
    // ToUint32 modulo.
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
        Some(Value::Boolean(true)) => 1,
        Some(Value::Boolean(false) | Value::Null) => 0,
        Some(Value::String(s)) => {
            let text = s.to_lossy_string();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                0
            } else {
                trimmed.parse::<i64>().map_or(0, |v| {
                    if v < 0 {
                        0
                    } else if v > u32::MAX as i64 {
                        u32::MAX
                    } else {
                        v as u32
                    }
                })
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
    gc_heap: &otter_gc::GcHeap,
    replacement_template: &[u16],
    string_heap: &crate::string::StringHeap,
) -> Result<Value, IntrinsicError> {
    let recv_units = recv.to_utf16_vec();
    let matches = collect_regex_matches(re, gc_heap, &recv_units);
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
    args: &mut IntrinsicArgs<'_>,
) -> Result<Value, IntrinsicError> {
    if limit == 0 {
        return Ok(Value::Array(args.array_from_elements_rooted(
            std::iter::empty(),
            &[],
            &[],
        )?));
    }
    let recv_units = recv.to_utf16_vec();
    let mut out: Vec<Value> = Vec::new();
    let mut cursor: usize = 0;
    let mut iter = re
        .find_from_utf16(&*args.gc_heap, &recv_units, 0)
        .into_iter();
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
            iter = re
                .find_from_utf16(&*args.gc_heap, &recv_units, cursor)
                .into_iter();
            continue;
        }
        let part =
            JsString::from_utf16_units(&recv_units[cursor..m.range.start], args.string_heap)?;
        out.push(Value::String(part));
        cursor = m.range.end;
    }
    if (out.len() as u32) < limit {
        let part = JsString::from_utf16_units(&recv_units[cursor..], args.string_heap)?;
        out.push(Value::String(part));
    }
    Ok(Value::Array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// §7.2.8 [`IsRegExp`](https://tc39.es/ecma262/#sec-isregexp): the
/// foundation surface treats any `Value::RegExp` as regex-shaped.
/// User objects can opt in via `[Symbol.match]` (a truthy property);
/// the intrinsic dispatcher does not currently invoke user `@@match`
/// traps, but the helper lights up the regex path so the rest of
/// the algorithm follows §22.1.3.13 / .14 / .15 / .17 / .18 / .19.
fn is_regexp_arg(arg: Option<&Value>) -> bool {
    matches!(arg, Some(Value::RegExp(_)))
}

/// §22.1.3.13 step 6 / §22.1.3.14 step 6 / §22.1.3.15 step 4: when
/// the first argument is not a `RegExp`, ToString-coerce it and run
/// `RegExpCreate(pattern, flags)`. The string fast-path matters for
/// idiomatic JS like `"foo".match("o+")` and `"foo".matchAll("o")`.
fn coerce_pattern_to_regexp(
    value: &Value,
    flags: &str,
    gc_heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
) -> Result<JsRegExp, IntrinsicError> {
    // §22.1.3.{13,14,15} step 6 — `pattern = ? ToString(arg)`.
    // Coerce every spec-relevant operand before compiling.
    let pattern_units: Vec<u16> = match value {
        Value::Undefined => Vec::new(),
        Value::String(s) => s.to_utf16_vec(),
        Value::Null => "null".encode_utf16().collect(),
        Value::Boolean(b) => {
            let text = if *b { "true" } else { "false" };
            text.encode_utf16().collect()
        }
        Value::Number(n) => n.to_display_string().encode_utf16().collect(),
        Value::BigInt(b) => b.to_decimal_string().encode_utf16().collect(),
        Value::Object(obj) => {
            let gc = &*gc_heap;
            if let Some(s) = crate::object::string_data(*obj, gc) {
                s.to_utf16_vec()
            } else if let Some(b) = crate::object::boolean_data(*obj, gc) {
                let text = if b { "true" } else { "false" };
                text.encode_utf16().collect()
            } else if let Some(n) = crate::object::number_data(*obj, gc) {
                n.to_display_string().encode_utf16().collect()
            } else {
                "[object Object]".encode_utf16().collect()
            }
        }
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a regular expression or string",
            });
        }
    };
    let _ = string_heap;
    JsRegExp::compile(gc_heap, &pattern_units, flags).map_err(|_| IntrinsicError::BadArgument {
        index: 0,
        reason: "is not a valid regular expression pattern",
    })
}

fn impl_match(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let coerced;
    let re = if is_regexp_arg(args.args.first()) {
        match args.args.first() {
            Some(Value::RegExp(r)) => r,
            _ => unreachable!(),
        }
    } else {
        let arg0 = args.args.first().unwrap_or(&Value::Undefined);
        let heap = &mut *args.gc_heap;
        coerced = coerce_pattern_to_regexp(arg0, "", heap, args.string_heap)?;
        &coerced
    };
    let recv_units = recv.to_utf16_vec();
    if re.flags(&*args.gc_heap).global {
        // `g` flag → return array of full matches only (no captures).
        let matches = collect_regex_matches(re, &*args.gc_heap, &recv_units);
        if matches.is_empty() {
            return Ok(Value::Null);
        }
        let mut out: Vec<Value> = Vec::with_capacity(matches.len());
        for m in &matches {
            let s = JsString::from_utf16_units(&recv_units[m.range.clone()], args.string_heap)?;
            out.push(Value::String(s));
        }
        return Ok(Value::Array(args.array_from_elements_rooted(
            out.iter().cloned(),
            &[],
            &[out.as_slice()],
        )?));
    }
    // Non-global → mirror `RegExp.prototype.exec` (carries
    // `index` / `input` / `groups` per §22.2.7.2).
    let m = match re
        .find_from_utf16(&*args.gc_heap, &recv_units, 0)
        .into_iter()
        .next()
    {
        Some(m) => m,
        None => return Ok(Value::Null),
    };
    let recv_clone = recv.clone();
    let has_indices = re.flags(&*args.gc_heap).has_indices;
    let arr = crate::regexp_prototype::build_match_result(
        &m,
        &recv_units,
        &recv_clone,
        has_indices,
        args,
        &[],
        &[],
    )?;
    Ok(Value::Array(arr))
}

fn impl_match_all(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let coerced;
    let re = if is_regexp_arg(args.args.first()) {
        let r = match args.args.first() {
            Some(Value::RegExp(r)) => r,
            _ => unreachable!(),
        };
        // §22.1.3.14 step 5.b: matchAll requires the global flag on
        // a RegExp arg; non-global is a TypeError.
        let heap = &*args.gc_heap;
        if !r.flags(heap).global {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a global regular expression",
            });
        }
        r
    } else {
        // §22.1.3.14 step 6.b: when the arg is not a RegExp, the
        // synthesised regex always has `g` set so the iteration
        // sweep visits every match.
        let arg0 = args.args.first().unwrap_or(&Value::Undefined);
        let heap = &mut *args.gc_heap;
        coerced = coerce_pattern_to_regexp(arg0, "g", heap, args.string_heap)?;
        &coerced
    };
    let recv_units = recv.to_utf16_vec();
    let matches = collect_regex_matches(re, &*args.gc_heap, &recv_units);
    let has_indices = re.flags(&*args.gc_heap).has_indices;
    let recv_clone = recv.clone();
    let mut out: Vec<Value> = Vec::with_capacity(matches.len());
    for m in &matches {
        let arr = crate::regexp_prototype::build_match_result(
            m,
            &recv_units,
            &recv_clone,
            has_indices,
            args,
            &[],
            &[out.as_slice()],
        )?;
        out.push(Value::Array(arr));
    }
    Ok(Value::Array(args.array_from_elements_rooted(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

fn impl_search(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let coerced;
    let re = if is_regexp_arg(args.args.first()) {
        match args.args.first() {
            Some(Value::RegExp(r)) => r,
            _ => unreachable!(),
        }
    } else {
        let arg0 = args.args.first().unwrap_or(&Value::Undefined);
        let heap = &mut *args.gc_heap;
        coerced = coerce_pattern_to_regexp(arg0, "", heap, args.string_heap)?;
        &coerced
    };
    let recv_units = recv.to_utf16_vec();
    // `search` always starts at index 0 — `lastIndex` is ignored
    // and not mutated per spec §22.1.3.13.
    let heap = &*args.gc_heap;
    let pos = re
        .find_from_utf16(heap, &recv_units, 0)
        .into_iter()
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
            "lastIndexOf"   / 2 => impl_last_index_of,
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
            // §B.2.3.2 / §B.2.3.3 — `trimLeft` is the AnnexB alias
            // for `trimStart`; `trimRight` is the alias for
            // `trimEnd`. Spec carries the same algorithm body, so
            // route both through the same intrinsic impls.
            "trimLeft"      / 0 => impl_trim_start,
            "trimRight"     / 0 => impl_trim_end,
            // §22.1.3.10 / .11 — Well-Formed Unicode Strings.
            "isWellFormed"  / 0 => impl_is_well_formed,
            "toWellFormed"  / 0 => impl_to_well_formed,
            // §B.2.3.1 AnnexB legacy substr(start, length).
            "substr"        / 2 => impl_substr,
            // §B.2.3 AnnexB HTML wrappers.
            "anchor"        / 1 => impl_anchor,
            "big"           / 0 => impl_big,
            "blink"         / 0 => impl_blink,
            "bold"          / 0 => impl_bold,
            "fixed"         / 0 => impl_fixed,
            "fontcolor"     / 1 => impl_fontcolor,
            "fontsize"      / 1 => impl_fontsize,
            "italics"       / 0 => impl_italics,
            "link"          / 1 => impl_link,
            "small"         / 0 => impl_small,
            "strike"        / 0 => impl_strike,
            "sub"           / 0 => impl_sub,
            "sup"           / 0 => impl_sup,
            "toLowerCase"   / 0 => impl_to_lower_case,
            "toUpperCase"   / 0 => impl_to_upper_case,
            // §22.1.3.21 / §22.1.3.23 — `toLocaleLowerCase` /
            // `toLocaleUpperCase` accept an optional `locales`
            // argument but their default behaviour matches their
            // locale-insensitive counterparts in the absence of an
            // Intl Locale impl. Until Intl lands, alias to the
            // generic case folders so the property exists and the
            // spec result-shape (a string of the same length plus
            // case mapping) holds for the ASCII fast path.
            "toLocaleLowerCase" / 0 => impl_to_lower_case,
            "toLocaleUpperCase" / 0 => impl_to_upper_case,
            "replace"       / 2 => impl_replace,
            "replaceAll"    / 2 => impl_replace_all,
            "split"         / 2 => impl_split,
            "match"         / 1 => impl_match,
            "matchAll"      / 1 => impl_match_all,
            "search"        / 1 => impl_search,
            "localeCompare" / 1 => impl_locale_compare,
            "normalize"     / 1 => impl_normalize,
            "toString"      / 0 => impl_to_string,
            "valueOf"       / 0 => impl_to_string,
        )
    });

/// §22.1.3.10 String.prototype.lastIndexOf(search, fromIndex?).
fn impl_last_index_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let needle = arg_to_string(args, 0)?;
    // ECMA-262 §22.1.3.11: `position` defaults to +∞, then
    // ToInteger, then min(pos, len). NaN clamps to 0. Foundation
    // takes the simpler accessor and clamps to `recv.len()`.
    let position = arg_u32_or(args, 1, recv.len())?.min(recv.len());
    let pos = recv
        .last_index_of(&needle, position, None)
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

/// §22.1.3.12 String.prototype.localeCompare. Foundation falls
/// back to spec-default Unicode code-point comparison; locale-
/// aware ordering ships through `Intl.Collator`.
fn impl_locale_compare(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?.to_lossy_string();
    let other = match args.args.first() {
        Some(Value::String(s)) => s.to_lossy_string(),
        Some(other) => other.display_string(),
        None => "undefined".to_string(),
    };
    let cmp = match recv.cmp(&other) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::Number(crate::number::NumberValue::from_i32(cmp)))
}

/// §22.1.3.13 String.prototype.normalize(form?). Foundation accepts
/// `"NFC"` / `"NFD"` / `"NFKC"` / `"NFKD"` (default `"NFC"`) and
/// returns the receiver string itself — the foundation slice does
/// not perform full Unicode normalisation but mirrors the spec
/// surface so call sites that depend on the method existing keep
/// working. Real normalisation lands alongside the ICU integration
/// follow-up.
fn impl_normalize(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    let form = match args.args.first() {
        None | Some(Value::Undefined) => "NFC".to_string(),
        Some(Value::String(s)) => s.to_lossy_string(),
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a string",
            });
        }
    };
    if !matches!(form.as_str(), "NFC" | "NFD" | "NFKC" | "NFKD") {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "must be one of NFC / NFD / NFKC / NFKD",
        });
    }
    Ok(Value::String(recv.clone()))
}

/// §22.1.3.27 String.prototype.toString — returns the primitive.
fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_string(args)?;
    Ok(Value::String(recv.clone()))
}

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    STRING_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::String, name)
}

/// Generic bridge that exposes a `String.prototype.<name>` intrinsic
/// as a JS-visible NativeFunction so user code reading the property
/// directly (`const f = "".split; f.call(s, ",")`) resolves to a
/// real callable. The compiler keeps its compile-time `CallString`
/// fast path; this bridge only services indirect access through the
/// own-property table.
///
/// The function captures the method name via a per-method
/// trampoline (see [`string_prototype_methods!`] below) and then
/// looks up the implementation in [`STRING_PROTOTYPE_TABLE`].
fn native_string_method(
    name: &'static str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    // §22.1.3.18 `replace` / §22.1.3.19 `replaceAll` — when the
    // replaceValue argument is callable, the intrinsic-table path
    // can't drive the replacement (no interpreter context). Intercept
    // here so `'abc'.replace('b', fn)` and `'abc'.replaceAll('b', fn)`
    // dispatch the callback with `(matched, position, string)` and
    // splice the result string.
    if (name == "replace" || name == "replaceAll")
        && args.len() >= 2
        && ctx.cx.interp.is_callable_runtime(args.get(1).unwrap())
        && matches!(args.first(), Some(Value::String(_)))
    {
        return native_string_replace_callable(name == "replaceAll", ctx, args);
    }
    let receiver = ctx.this_value().clone();
    // §22.1.3.* String.prototype.* int / string arg coercion.
    // Mirrors the `Op::CallMethodValue` String arm in
    // `method_ops.rs` so `.call(...)` / `.apply(...)` invocations
    // run the spec `ToIntegerOrInfinity` / `ToString` ladders on
    // non-primitive operands and observe user `@@toPrimitive` /
    // `valueOf` / `toString`.
    let (string_int_coerce, string_str_coerce): (&[usize], &[usize]) = match name {
        "indexOf" | "lastIndexOf" | "includes" | "startsWith" | "endsWith" => (&[1], &[0]),
        "slice" | "substring" | "substr" => (&[0, 1], &[]),
        "at" | "charAt" | "charCodeAt" | "codePointAt" => (&[0], &[]),
        "repeat" => (&[0], &[]),
        "padStart" | "padEnd" => (&[0], &[1]),
        "replace" | "replaceAll" => (&[], &[0]),
        "split" => (&[1], &[0]),
        "concat" => (&[], &[0, 1, 2, 3]),
        _ => (&[], &[]),
    };
    let coerced_args: smallvec::SmallVec<[Value; 4]> =
        if string_int_coerce.is_empty() && string_str_coerce.is_empty() {
            args.iter().cloned().collect()
        } else {
            let mut out: smallvec::SmallVec<[Value; 4]> = args.iter().cloned().collect();
            if let Some(exec) = ctx.execution_context().cloned() {
                let is_non_primitive = |v: &Value| {
                    matches!(
                        v,
                        Value::Object(_)
                            | Value::Array(_)
                            | Value::Function { .. }
                            | Value::Closure { .. }
                            | Value::NativeFunction(_)
                            | Value::BoundFunction(_)
                            | Value::ClassConstructor(_)
                            | Value::Proxy(_)
                            | Value::RegExp(_)
                    )
                };
                for &idx in string_int_coerce {
                    let Some(slot) = out.get_mut(idx) else {
                        continue;
                    };
                    if !is_non_primitive(slot) {
                        continue;
                    }
                    let interp = ctx.interp_mut();
                    let primitive = interp
                        .evaluate_to_primitive(
                            &exec,
                            slot,
                            crate::abstract_ops::ToPrimitiveHint::Number,
                        )
                        .map_err(|e| NativeError::TypeError {
                            name,
                            reason: e.to_string(),
                        })?;
                    *slot = primitive;
                }
                for &idx in string_str_coerce {
                    let Some(slot) = out.get_mut(idx) else {
                        continue;
                    };
                    if !is_non_primitive(slot) {
                        continue;
                    }
                    let interp = ctx.interp_mut();
                    let primitive = interp
                        .evaluate_to_primitive(
                            &exec,
                            slot,
                            crate::abstract_ops::ToPrimitiveHint::String,
                        )
                        .map_err(|e| NativeError::TypeError {
                            name,
                            reason: e.to_string(),
                        })?;
                    *slot = primitive;
                }
            }
            out
        };
    let (string_heap, allocation_roots) = {
        let interp = ctx.interp_mut();
        (interp.string_heap_clone(), interp.collect_runtime_roots())
    };
    let entry = lookup(name).ok_or_else(|| NativeError::TypeError {
        name,
        reason: "unknown String.prototype method".to_string(),
    })?;
    (entry.impl_fn)(&mut IntrinsicArgs {
        receiver: &receiver,
        args: &coerced_args,
        string_heap: &string_heap,
        gc_heap: ctx.heap_mut(),
        allocation_roots: allocation_roots.as_slice(),
    })
    .map_err(|err| match err {
        IntrinsicError::OutOfRange { .. } => NativeError::RangeError {
            name,
            reason: err.to_string(),
        },
        _ => NativeError::TypeError {
            name,
            reason: err.to_string(),
        },
    })
}

/// Drive `String.prototype.replace` / `replaceAll` when
/// `replaceValue` is callable. Walks the receiver's UTF-16 units in
/// place, locates each non-overlapping match of the
/// string-coerced needle, invokes the callback with
/// `(matched, position, fullString)` per §22.1.3.{18,19} step 6.h,
/// and splices the returned string back in.
fn native_string_replace_callable(
    replace_all: bool,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let receiver = ctx.this_value().clone();
    let string_heap = ctx.interp_mut().string_heap_clone();
    let intrinsic_args = IntrinsicArgs {
        receiver: &receiver,
        args,
        string_heap: &string_heap,
        gc_heap: ctx.heap_mut(),
        allocation_roots: &[],
    };
    let recv = receiver_string(&intrinsic_args).map_err(|err| NativeError::TypeError {
        name: if replace_all { "replaceAll" } else { "replace" },
        reason: err.to_string(),
    })?;
    let needle = match args.first() {
        Some(Value::String(s)) => s.clone(),
        _ => unreachable!("guarded by caller — args[0] is Value::String"),
    };
    let callback = args.get(1).cloned().unwrap_or(Value::Undefined);
    let recv_units = recv.to_utf16_vec();
    let needle_units = needle.to_utf16_vec();
    let needle_len = needle_units.len();
    let recv_str = recv.clone();
    let recv_value = Value::String(recv_str);
    let context = ctx.execution_context().cloned();
    let context = match context {
        Some(c) => c,
        None => {
            return Err(NativeError::TypeError {
                name: if replace_all { "replaceAll" } else { "replace" },
                reason: "missing execution context".to_string(),
            });
        }
    };
    let interp = ctx.interp_mut();
    let mut out: Vec<u16> = Vec::with_capacity(recv_units.len());
    let mut cursor: usize = 0;
    // Edge case: empty needle splices the callback result at every
    // unit boundary plus the end (matches §22.1.3.19 step 12.b /
    // §22.1.3.18 step 12.b).
    if needle_len == 0 {
        let positions: Vec<usize> = if replace_all {
            (0..=recv_units.len()).collect()
        } else {
            vec![0]
        };
        for pos in positions {
            let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                Value::String(needle.clone()),
                Value::Number(crate::number::NumberValue::from_f64(pos as f64)),
                recv_value.clone(),
            ];
            let raw = interp
                .run_callable_sync(&context, &callback, Value::Undefined, cb_args)
                .map_err(|err| NativeError::TypeError {
                    name: if replace_all { "replaceAll" } else { "replace" },
                    reason: err.to_string(),
                })?;
            let raw_string =
                match raw {
                    Value::String(s) => s,
                    other => JsString::from_str(&other.display_string(), &string_heap).map_err(
                        |err| NativeError::TypeError {
                            name: if replace_all { "replaceAll" } else { "replace" },
                            reason: err.to_string(),
                        },
                    )?,
                };
            out.extend_from_slice(&raw_string.to_utf16_vec());
            if pos < recv_units.len() {
                out.push(recv_units[pos]);
            }
        }
        return Ok(Value::String(
            JsString::from_utf16_units(&out, &string_heap).map_err(|err| {
                NativeError::TypeError {
                    name: if replace_all { "replaceAll" } else { "replace" },
                    reason: err.to_string(),
                }
            })?,
        ));
    }
    let last_start = recv_units.len().saturating_sub(needle_len);
    while cursor <= last_start {
        if recv_units[cursor..cursor + needle_len] == needle_units[..] {
            let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                Value::String(needle.clone()),
                Value::Number(crate::number::NumberValue::from_f64(cursor as f64)),
                recv_value.clone(),
            ];
            let raw = interp
                .run_callable_sync(&context, &callback, Value::Undefined, cb_args)
                .map_err(|err| NativeError::TypeError {
                    name: if replace_all { "replaceAll" } else { "replace" },
                    reason: err.to_string(),
                })?;
            let raw_string =
                match raw {
                    Value::String(s) => s,
                    other => JsString::from_str(&other.display_string(), &string_heap).map_err(
                        |err| NativeError::TypeError {
                            name: if replace_all { "replaceAll" } else { "replace" },
                            reason: err.to_string(),
                        },
                    )?,
                };
            out.extend_from_slice(&raw_string.to_utf16_vec());
            cursor += needle_len;
            if !replace_all {
                break;
            }
        } else {
            out.push(recv_units[cursor]);
            cursor += 1;
        }
    }
    out.extend_from_slice(&recv_units[cursor..]);
    Ok(Value::String(
        JsString::from_utf16_units(&out, &string_heap).map_err(|err| NativeError::TypeError {
            name: if replace_all { "replaceAll" } else { "replace" },
            reason: err.to_string(),
        })?,
    ))
}

/// Generate a per-method trampoline + spec-table entry. The
/// trampoline binds the JavaScript method name into a `fn`-pointer
/// shape that fits `NativeCall::Static` without dynamic dispatch.
macro_rules! string_prototype_methods {
    ($($bridge:ident => $name:literal, $length:literal;)*) => {
        $(
            fn $bridge(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
                native_string_method($name, ctx, args)
            }
        )*

        /// Declarative `String.prototype` method specs installed as
        /// JS-visible own properties during the `String` bootstrap.
        ///
        /// Property-access surfaces (`"".split`, `Reflect.get`,
        /// `Object.getOwnPropertyNames(String.prototype)`) resolve
        /// through this table; the compile-time `CallString` fast
        /// path keeps using [`STRING_PROTOTYPE_TABLE`] directly.
        pub static STRING_PROTOTYPE_METHODS: &[MethodSpec] = &[
            $(MethodSpec {
                name: $name,
                length: $length,
                attrs: Attr::builtin_function(),
                call: NativeCall::Static($bridge),
            },)*
        ];
    };
}

string_prototype_methods!(
    bridge_char_at         => "charAt",         1;
    bridge_char_code_at    => "charCodeAt",     1;
    bridge_code_point_at   => "codePointAt",    1;
    bridge_at              => "at",             1;
    bridge_slice           => "slice",          2;
    bridge_substring       => "substring",      2;
    bridge_index_of        => "indexOf",        2;
    bridge_last_index_of   => "lastIndexOf",    2;
    bridge_includes        => "includes",       2;
    bridge_starts_with     => "startsWith",     2;
    bridge_ends_with       => "endsWith",       2;
    bridge_concat          => "concat",         1;
    bridge_repeat          => "repeat",         1;
    bridge_pad_start       => "padStart",       2;
    bridge_pad_end         => "padEnd",         2;
    bridge_trim            => "trim",           0;
    bridge_trim_start      => "trimStart",      0;
    bridge_trim_end        => "trimEnd",        0;
    bridge_trim_left       => "trimLeft",       0;
    bridge_trim_right      => "trimRight",      0;
    bridge_is_well_formed  => "isWellFormed",   0;
    bridge_to_well_formed  => "toWellFormed",   0;
    bridge_substr          => "substr",         2;
    bridge_anchor          => "anchor",         1;
    bridge_big             => "big",            0;
    bridge_blink           => "blink",          0;
    bridge_bold            => "bold",           0;
    bridge_fixed           => "fixed",          0;
    bridge_fontcolor       => "fontcolor",      1;
    bridge_fontsize        => "fontsize",       1;
    bridge_italics         => "italics",        0;
    bridge_link            => "link",           1;
    bridge_small           => "small",          0;
    bridge_strike          => "strike",         0;
    bridge_sub             => "sub",            0;
    bridge_sup             => "sup",            0;
    bridge_to_lower_case   => "toLowerCase",    0;
    bridge_to_upper_case   => "toUpperCase",    0;
    bridge_to_locale_lower => "toLocaleLowerCase", 0;
    bridge_to_locale_upper => "toLocaleUpperCase", 0;
    bridge_replace         => "replace",        2;
    bridge_replace_all     => "replaceAll",     2;
    bridge_split           => "split",          2;
    bridge_match           => "match",          1;
    bridge_match_all       => "matchAll",       1;
    bridge_search          => "search",         1;
    bridge_locale_compare  => "localeCompare",  1;
    bridge_normalize       => "normalize",      1;
    bridge_to_string       => "toString",       0;
    bridge_value_of        => "valueOf",        0;
);

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
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv_v = Value::String(JsString::from_str(recv, &heap).unwrap());
        let arg_vs: Vec<Value> = args
            .iter()
            .map(|s| match s.parse::<i32>() {
                Ok(n) => Value::Number(NumberValue::from_i32(n)),
                Err(_) => Value::String(JsString::from_str(s, &heap).unwrap()),
            })
            .collect();
        let entry = lookup(method).unwrap();
        let result = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv_v,
            args: &arg_vs,
            string_heap: &heap,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
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
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let entry = lookup("length").unwrap();
        let err = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &Value::Undefined,
            args: &[],
            string_heap: &heap,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
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
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        call_v_with_heap(method, recv, args, &heap, &mut gc_heap)
    }

    fn call_v_with_heap(
        method: &str,
        recv: &str,
        args: &[A],
        heap: &StringHeap,
        gc_heap: &mut otter_gc::GcHeap,
    ) -> Value {
        let recv_v = Value::String(JsString::from_str(recv, &heap).unwrap());
        let arg_vs: Vec<Value> = args
            .iter()
            .map(|a| match a {
                A::N(n) => Value::Number(NumberValue::from_i32(*n)),
                A::S(s) => Value::String(JsString::from_str(s, &heap).unwrap()),
            })
            .collect();
        let entry = lookup(method).unwrap();
        (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv_v,
            args: &arg_vs,
            string_heap: &heap,
            gc_heap,
            allocation_roots: &[],
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
    fn concat_coerces_non_string_args() {
        // §22.1.3.5 — `for each next of arguments: nextString = ?
        // ToString(next)`. Numbers, Booleans, etc. coerce instead
        // of rejecting.
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv = Value::String(JsString::from_str("a", &heap).unwrap());
        let entry = lookup("concat").unwrap();
        let result = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv,
            args: &[Value::Number(NumberValue::from_i32(3))],
            string_heap: &heap,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .unwrap();
        match result {
            Value::String(s) => assert_eq!(s.to_lossy_string(), "a3"),
            other => panic!("expected string result, got {other:?}"),
        }
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
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv = Value::String(JsString::from_str("abc", &heap).unwrap());
        let entry = lookup("repeat").unwrap();
        let err = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv,
            args: &[Value::Number(NumberValue::from_i32(-1))],
            string_heap: &heap,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
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
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let units: [u16; 3] = [0xD800, 0xDC00, b'a' as u16];
        let recv = Value::String(JsString::from_utf16_units(&units, &heap).unwrap());
        let entry = lookup("codePointAt").unwrap();
        let r = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv,
            args: &[Value::Number(NumberValue::from_i32(0))],
            string_heap: &heap,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .unwrap();
        assert_eq!(r.display_string(), "65536");
        // Index 1 is the trailing surrogate alone.
        let r2 = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv,
            args: &[Value::Number(NumberValue::from_i32(1))],
            string_heap: &heap,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
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
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let r = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv,
            args: &[],
            string_heap: &heap,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
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
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let v = call_v_with_heap("split", "a,b,c", &[A::S(",")], &heap, &mut gc_heap);
        match v {
            Value::Array(a) => {
                assert_eq!(crate::array::len(a, &gc_heap), 3);
                assert_eq!(crate::array::get(a, &gc_heap, 0).display_string(), "a");
                assert_eq!(crate::array::get(a, &gc_heap, 1).display_string(), "b");
                assert_eq!(crate::array::get(a, &gc_heap, 2).display_string(), "c");
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_consecutive_separators_yield_empty_chunks() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let v = call_v_with_heap("split", "a,,b", &[A::S(",")], &heap, &mut gc_heap);
        match v {
            Value::Array(a) => {
                assert_eq!(crate::array::len(a, &gc_heap), 3);
                assert_eq!(crate::array::get(a, &gc_heap, 0).display_string(), "a");
                assert_eq!(crate::array::get(a, &gc_heap, 1).display_string(), "");
                assert_eq!(crate::array::get(a, &gc_heap, 2).display_string(), "b");
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_empty_separator_yields_code_units() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let v = call_v_with_heap("split", "abc", &[A::S("")], &heap, &mut gc_heap);
        match v {
            Value::Array(a) => {
                assert_eq!(crate::array::len(a, &gc_heap), 3);
                assert_eq!(crate::array::get(a, &gc_heap, 0).display_string(), "a");
                assert_eq!(crate::array::get(a, &gc_heap, 1).display_string(), "b");
                assert_eq!(crate::array::get(a, &gc_heap, 2).display_string(), "c");
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_with_limit() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let v = call_v_with_heap(
            "split",
            "a,b,c,d",
            &[A::S(","), A::N(2)],
            &heap,
            &mut gc_heap,
        );
        match v {
            Value::Array(a) => {
                assert_eq!(crate::array::len(a, &gc_heap), 2);
                assert_eq!(crate::array::get(a, &gc_heap, 0).display_string(), "a");
                assert_eq!(crate::array::get(a, &gc_heap, 1).display_string(), "b");
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_no_match_returns_singleton() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let v = call_v_with_heap("split", "abc", &[A::S(",")], &heap, &mut gc_heap);
        match v {
            Value::Array(a) => {
                assert_eq!(crate::array::len(a, &gc_heap), 1);
                assert_eq!(crate::array::get(a, &gc_heap, 0).display_string(), "abc");
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_empty_receiver() {
        // "".split(",") === [""]
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let v = call_v_with_heap("split", "", &[A::S(",")], &heap, &mut gc_heap);
        match v {
            Value::Array(a) => {
                assert_eq!(crate::array::len(a, &gc_heap), 1);
                assert_eq!(crate::array::get(a, &gc_heap, 0).display_string(), "");
            }
            _ => panic!("expected array"),
        }
        // "".split("") === []
        let v2 = call_v_with_heap("split", "", &[A::S("")], &heap, &mut gc_heap);
        match v2 {
            Value::Array(a) => assert_eq!(crate::array::len(a, &gc_heap), 0),
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn split_undefined_separator_returns_singleton() {
        // "abc".split() === ["abc"]
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv = Value::String(JsString::from_str("abc", &heap).unwrap());
        let entry = lookup("split").unwrap();
        let r = (entry.impl_fn)(&mut IntrinsicArgs {
            receiver: &recv,
            args: &[],
            string_heap: &heap,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .unwrap();
        match r {
            Value::Array(a) => {
                assert_eq!(crate::array::len(a, &gc_heap), 1);
                assert_eq!(crate::array::get(a, &gc_heap, 0).display_string(), "abc");
            }
            _ => panic!("expected array"),
        }
    }
}
