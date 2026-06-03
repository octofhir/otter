//! `String.prototype.*` native implementations.
//!
//! Primitive string receivers reach these methods through the
//! JS-visible `String.prototype` native functions installed by the
//! `String` `couch!` surface.
//!
//! # Contents
//! - [`STRING_PROTOTYPE_METHODS`] — native method specs installed on
//!   the global `String.prototype`.
//! - One private `impl_*` function per method.
//!
//! # Invariants
//! - Every method validates the receiver as a primitive string or a
//!   String wrapper with `[[StringData]]`; a non-string raises
//!   `TypeError`.
//! - Numeric arguments accept `Value::Number` and (for foundation-era
//!   ergonomics on a few methods) string-encoded indices.
//! - `indexOf` polls the runtime interrupt flag every
//!   [`crate::string::INDEX_OF_INTERRUPT_BUDGET`] iterations.
//! - `toLowerCase` / `toUpperCase` are **ASCII-only**. Full Unicode
//!   case folding is deferred until ICU integration; non-ASCII code
//!   units pass through unchanged.
//! - `replace` / `replaceAll` / `split` / `match` / `matchAll` /
//!   `search` run the full §22.1.3 ladder: an Object argument
//!   delegates to its `@@replace` / `@@split` / `@@match` /
//!   `@@matchAll` / `@@search` method; the string-search paths honour
//!   functional replacers and the `$$` / `$&` / `` $` `` / `$'`
//!   substitution patterns.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-string-prototype-object>

use smallvec::SmallVec;

use crate::Value;
use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::number::NumberValue;
use crate::regexp::JsRegExp;
use crate::string::Interrupted;
use crate::string::JsString;
use crate::symbol::WellKnown;
use crate::{NativeCtx, NativeError, VmGetOutcome, VmPropertyKey};

fn type_error(name: &'static str, reason: impl Into<String>) -> NativeError {
    NativeError::TypeError {
        name,
        reason: reason.into(),
    }
}

fn range_error(name: &'static str, reason: impl Into<String>) -> NativeError {
    NativeError::RangeError {
        name,
        reason: reason.into(),
    }
}

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
fn receiver_string(ctx: &mut NativeCtx<'_>, receiver: &Value) -> Result<JsString, NativeError> {
    let recv = receiver;
    if let Some(s) = recv.as_string(ctx.heap_mut()) {
        return Ok(s);
    }
    if let Some(obj) = recv.as_object() {
        let gc = ctx.heap();
        if let Some(s) = crate::object::string_data(obj, gc) {
            return Ok(s);
        }
        if let Some(b) = crate::object::boolean_data(obj, gc) {
            let text = if b { "true" } else { "false" };
            return Ok(JsString::from_str(text, ctx.heap_mut())?);
        }
        if let Some(n) = crate::object::number_data(obj, gc) {
            let text = n.to_display_string();
            return Ok(JsString::from_str(&text, ctx.heap_mut())?);
        }
        return Ok(JsString::from_str("[object Object]", ctx.heap_mut())?);
    }
    if let Some(b) = recv.as_boolean() {
        let text = if b { "true" } else { "false" };
        return Ok(JsString::from_str(text, ctx.heap_mut())?);
    }
    if let Some(n) = recv.as_number() {
        let text = n.to_display_string();
        return Ok(JsString::from_str(&text, ctx.heap_mut())?);
    }
    if let Some(b) = recv.as_big_int() {
        let text = b.to_decimal_string(ctx.heap());
        return Ok(JsString::from_str(&text, ctx.heap_mut())?);
    }
    if let Some(arr) = recv.as_array() {
        // §22.1.3.32 Array.prototype.toString → Array.prototype.join(",").
        let gc = ctx.heap();
        let items: Vec<String> = crate::array::with_elements(arr, gc, |els| {
            els.iter()
                .map(|v| {
                    if v.is_null() || v.is_undefined() || v.is_hole() {
                        String::new()
                    } else if let Some(s) = v.as_string(gc) {
                        s.to_lossy_string(gc)
                    } else if let Some(n) = v.as_number() {
                        n.to_display_string()
                    } else if let Some(b) = v.as_boolean() {
                        if b { "true" } else { "false" }.to_string()
                    } else {
                        v.display_string(gc)
                    }
                })
                .collect()
        });
        return Ok(JsString::from_str(&items.join(","), ctx.heap_mut())?);
    }
    if let Some(re) = recv.as_regexp() {
        // §22.2.6.13 RegExp.prototype.toString — `/source/flags`.
        let gc = ctx.heap();
        let pattern = re.source(gc);
        let flags = re.flags(gc);
        let pattern_str = if pattern.is_empty() {
            "(?:)".to_string()
        } else {
            pattern
        };
        let text = format!("/{}/{}", pattern_str, flags.to_js_string());
        return Ok(JsString::from_str(&text, ctx.heap_mut())?);
    }
    Err(type_error("String.prototype", "expected string"))
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
fn arg_to_string(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    index: u16,
) -> Result<JsString, NativeError> {
    let Some(arg) = args.get(index as usize) else {
        return Ok(JsString::undefined_str(ctx.heap_mut())?);
    };
    if arg.is_undefined() {
        return Ok(JsString::undefined_str(ctx.heap_mut())?);
    }
    if arg.is_null() {
        return Ok(JsString::null_str(ctx.heap_mut())?);
    }
    if let Some(s) = arg.as_string(ctx.heap_mut()) {
        return Ok(s);
    }
    if let Some(b) = arg.as_boolean() {
        let text = if b { "true" } else { "false" };
        return Ok(JsString::from_str(text, ctx.heap_mut())?);
    }
    if let Some(n) = arg.as_number() {
        let text = n.to_display_string();
        return Ok(JsString::from_str(&text, ctx.heap_mut())?);
    }
    if let Some(b) = arg.as_big_int() {
        let text = b.to_decimal_string(ctx.heap());
        return Ok(JsString::from_str(&text, ctx.heap_mut())?);
    }
    if let Some(obj) = arg.as_object() {
        let gc = ctx.heap();
        return crate::object::string_data(obj, gc)
            .ok_or_else(|| type_error("String.prototype", "must be a string"));
    }
    if arg.is_symbol() {
        return Err(type_error(
            "String.prototype",
            "Symbol values cannot be converted to a string",
        ));
    }
    Err(type_error("String.prototype", "must be a string"))
}

/// Pull a u32 index from arg `index`. §7.1.5 ToUint32 coerces every
/// spec-relevant operand: `Value::Number` clamps to `[0, u32::MAX]`,
/// `Value::Boolean` (true → 1, false → 0), `Value::Null` → 0,
/// `Value::String` parses as decimal (NaN-on-failure clamps to 0
/// per ToUint32 modulo), `Value::Undefined` and missing arguments
/// collapse to `default`.
fn arg_u32_or(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    index: u16,
    default: u32,
) -> Result<u32, NativeError> {
    let Some(arg) = args.get(index as usize) else {
        return Ok(default);
    };
    if arg.is_undefined() {
        return Ok(default);
    }
    if let Some(n) = arg.as_number() {
        return Ok(number_to_u32(n));
    }
    if let Some(b) = arg.as_boolean() {
        return Ok(if b { 1 } else { 0 });
    }
    if arg.is_null() {
        return Ok(0);
    }
    if let Some(s) = arg.as_string(ctx.heap_mut()) {
        return Ok(parse_index(s, ctx.heap_mut()).unwrap_or(0));
    }
    Err(type_error(
        "String.prototype",
        "must be a non-negative integer",
    ))
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

fn parse_index(s: JsString, heap: &otter_gc::GcHeap) -> Option<u32> {
    let text = s.to_lossy_string(heap);
    text.trim().parse::<u32>().ok()
}

/// Pull a signed integer (negative-tolerant) from arg `index`.
/// Mirrors `ToIntegerOrInfinity` for the foundation subset:
/// `NaN`/missing/`undefined` → `default`; non-finite values clamp
/// to [`i64::MIN`] / [`i64::MAX`]; finite floats truncate toward
/// zero.
fn arg_int_or(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    index: u16,
    default: i64,
) -> Result<i64, NativeError> {
    // §7.1.5 ToIntegerOrInfinity — coerce the spec-relevant operand
    // set (Number / Boolean / Null / String) before treating
    // non-finite / NaN as the default.
    let Some(arg) = args.get(index as usize) else {
        return Ok(default);
    };
    if arg.is_undefined() {
        return Ok(default);
    }
    if let Some(n) = arg.as_number() {
        return Ok(number_to_int(n));
    }
    if let Some(b) = arg.as_boolean() {
        return Ok(if b { 1 } else { 0 });
    }
    if arg.is_null() {
        return Ok(0);
    }
    if let Some(s) = arg.as_string(ctx.heap_mut()) {
        let text = s.to_lossy_string(ctx.heap_mut());
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(0);
        }
        return Ok(trimmed.parse::<i64>().unwrap_or(0));
    }
    Err(type_error("String.prototype", "must be a number"))
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

fn impl_length(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    Ok(Value::number(NumberValue::from_i32(recv.len() as i32)))
}

fn impl_char_code_at(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let idx = arg_u32_or(ctx, args, 0, 0)?;
    let value = match recv.char_code_at(idx, ctx.heap_mut()) {
        Some(unit) => NumberValue::from_i32(i32::from(unit)),
        None => NumberValue::Double(f64::NAN),
    };
    Ok(Value::number(value))
}

fn impl_char_at(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    // §22.1.3.1 — position is `ToIntegerOrInfinity(pos)` (signed): a
    // negative or out-of-range index returns the empty string rather
    // than clamping to index 0, so `"abc".charAt(-1)` is `""`.
    let pos = arg_int_or(ctx, args, 0, 0)?;
    let len = recv.len() as i64;
    if pos < 0 || pos >= len {
        return Ok(Value::string(JsString::empty(ctx.heap_mut())?));
    }
    match recv.char_code_at(pos as u32, ctx.heap_mut()) {
        Some(u) => {
            let s = JsString::from_utf16_units(&[u], ctx.heap_mut())?;
            Ok(Value::string(s))
        }
        None => Ok(Value::string(JsString::empty(ctx.heap_mut())?)),
    }
}

fn impl_slice(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    // §22.1.3.20 — `intStart` / `intEnd` are ToIntegerOrInfinity, so a
    // negative index counts from the end (`max(len + n, 0)`) rather than
    // clamping to zero; only then is the span taken.
    let recv = receiver_string(ctx, receiver)?;
    let total = recv.len() as i64;
    let int_start = arg_int_or(ctx, args, 0, 0)?;
    let from = if int_start < 0 {
        (total + int_start).max(0)
    } else {
        int_start.min(total)
    };
    let int_end = arg_int_or(ctx, args, 1, total)?;
    let to = if int_end < 0 {
        (total + int_end).max(0)
    } else {
        int_end.min(total)
    };
    let length = (to - from).max(0);
    let out = recv.slice(from as u32, length as u32, ctx.heap_mut())?;
    Ok(Value::string(out))
}

fn impl_substring(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let total = recv.len();
    let mut start = arg_u32_or(ctx, args, 0, 0)?.min(total);
    let mut end = match args.get(1) {
        Some(_) => arg_u32_or(ctx, args, 1, total)?.min(total),
        None => total,
    };
    // Spec: if start > end, swap.
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    let length = end - start;
    let out = recv.slice(start, length, ctx.heap_mut())?;
    Ok(Value::string(out))
}

fn impl_index_of(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let needle = arg_to_string(ctx, args, 0)?;
    let from = arg_u32_or(ctx, args, 1, 0)?;
    let pos = recv
        .index_of(needle, from, None, ctx.heap_mut())
        .map_err(|Interrupted| type_error("String.prototype", "interrupted"))?;
    let value = match pos {
        Some(p) => NumberValue::from_i32(p as i32),
        None => NumberValue::from_i32(-1),
    };
    Ok(Value::number(value))
}

fn impl_starts_with(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let search = args.first().copied().unwrap_or(Value::undefined());
    // §22.1.3.21 step 4-6 — a RegExp searchString throws before its ToString.
    if is_reg_exp(ctx, search, "String.prototype.startsWith")? {
        return Err(type_error(
            "String.prototype.startsWith",
            "searchString must not be a RegExp",
        ));
    }
    let needle = value_to_string(ctx, search, "String.prototype.startsWith")?;
    let from = arg_u32_or(ctx, args, 1, 0)?;
    Ok(Value::boolean(recv.starts_with(
        needle,
        from,
        ctx.heap_mut(),
    )))
}

fn impl_ends_with(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let search = args.first().copied().unwrap_or(Value::undefined());
    // §22.1.3.7 step 4-6 — a RegExp searchString throws before its ToString.
    if is_reg_exp(ctx, search, "String.prototype.endsWith")? {
        return Err(type_error(
            "String.prototype.endsWith",
            "searchString must not be a RegExp",
        ));
    }
    let needle = value_to_string(ctx, search, "String.prototype.endsWith")?;
    let end_pos = arg_u32_or(ctx, args, 1, recv.len())?;
    Ok(Value::boolean(recv.ends_with(
        needle,
        end_pos,
        ctx.heap_mut(),
    )))
}

fn impl_includes(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let search = args.first().copied().unwrap_or(Value::undefined());
    // §22.1.3.7 step 4-6 — a RegExp searchString throws before its ToString.
    if is_reg_exp(ctx, search, "String.prototype.includes")? {
        return Err(type_error(
            "String.prototype.includes",
            "searchString must not be a RegExp",
        ));
    }
    let needle = value_to_string(ctx, search, "String.prototype.includes")?;
    let from = arg_u32_or(ctx, args, 1, 0)?;
    let pos = recv
        .index_of(needle, from, None, ctx.heap_mut())
        .map_err(|Interrupted| type_error("String.prototype", "interrupted"))?;
    Ok(Value::boolean(pos.is_some()))
}

fn impl_concat(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    // §22.1.3.5 — `for each next of arguments: nextString = ?
    // ToString(next)`. Coerce every operand via the shared
    // `arg_to_string` helper (primitives + wrapper objects with
    // `[[StringData]]`); plain objects without an inherited
    // `toString` still reject.
    let recv = receiver_string(ctx, receiver)?;
    let mut result = recv;
    for i in 0..args.len() {
        let piece = arg_to_string(ctx, args, i as u16)?;
        result = JsString::concat(result, piece, ctx.heap_mut())?;
    }
    Ok(Value::string(result))
}

fn impl_repeat(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    // §22.1.3.18 steps 3-4 — n = ToIntegerOrInfinity(count); a negative
    // or `+∞` count is a RangeError, checked BEFORE the `n = 0` /
    // empty-string shortcuts so `"".repeat(Infinity)` and
    // `"".repeat(-1)` throw rather than returning `""`. The argument is
    // pre-coerced to a Number by the String method coercion pass.
    let raw = match args.first() {
        Some(v) if !v.is_undefined() => v.as_number().map_or(0.0, |n| n.as_f64()),
        _ => 0.0,
    };
    let n = if raw.is_nan() { 0.0 } else { raw.trunc() };
    if n < 0.0 || n == f64::INFINITY {
        return Err(range_error(
            "String.prototype.repeat",
            "count must be a non-negative finite number",
        ));
    }
    let count = n as i64;
    if count == 0 || recv.is_empty() {
        return Ok(Value::string(JsString::empty(ctx.heap_mut())?));
    }
    let units = recv.to_utf16_vec(ctx.heap_mut());
    let total = (units.len() as u64).saturating_mul(count as u64);
    if total > u32::MAX as u64 {
        return Err(range_error(
            "String.prototype",
            "result would exceed maximum string length",
        ));
    }
    let mut buf = Vec::with_capacity(total as usize);
    for _ in 0..count {
        buf.extend_from_slice(&units);
    }
    Ok(Value::string(JsString::from_utf16_units(
        &buf,
        ctx.heap_mut(),
    )?))
}

/// Pad-direction selector for [`pad_impl`].
#[derive(Clone, Copy)]
enum PadSide {
    Start,
    End,
}

fn pad_impl(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
    side: PadSide,
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let target = arg_int_or(ctx, args, 0, 0)?;
    let recv_len = recv.len() as i64;
    if target <= recv_len {
        return Ok(Value::string(recv));
    }
    // §22.1.3.16 step 11 / §22.1.3.17 step 11 — `fillString` is
    // either `undefined` (single-space default) or the result of
    // `ToString(fillString)`. Coerce every spec-relevant operand
    // shape through `arg_to_string` so primitive fill strings
    // round-trip without bailing.
    let pad_units: Vec<u16> = match args.get(1) {
        None => vec![0x0020],
        Some(v) if v.is_undefined() => vec![0x0020],
        _ => arg_to_string(ctx, args, 1)?.to_utf16_vec(ctx.heap_mut()),
    };
    if pad_units.is_empty() {
        return Ok(Value::string(recv));
    }
    let pad_count = (target - recv_len) as usize;
    let recv_units = recv.to_utf16_vec(ctx.heap_mut());
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
    Ok(Value::string(JsString::from_utf16_units(
        &buf,
        ctx.heap_mut(),
    )?))
}

fn impl_pad_start(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    pad_impl(ctx, receiver, args, PadSide::Start)
}

fn impl_pad_end(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    pad_impl(ctx, receiver, args, PadSide::End)
}

/// Trim-direction selector for [`trim_impl`].
#[derive(Clone, Copy)]
enum TrimSide {
    Both,
    Start,
    End,
}

fn trim_impl(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    _args: &[Value],
    side: TrimSide,
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let units = recv.to_utf16_vec(ctx.heap_mut());
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
    Ok(Value::string(JsString::from_utf16_units(
        slice,
        ctx.heap_mut(),
    )?))
}

fn impl_trim(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    trim_impl(ctx, receiver, args, TrimSide::Both)
}

/// §B.2.3.1 `CreateHTML(string, tag, attribute, value)`.
///
/// Wraps the receiver string in an HTML tag, optionally with a
/// single `attribute="value"` pair (double-quoted, `"` in the value
/// escaped to `&quot;`). Implements the foundation of the
/// `String.prototype.{anchor, big, blink, bold, fixed, fontcolor,
/// fontsize, italics, link, small, strike, sub, sup}` AnnexB shims.
fn create_html(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
    tag: &str,
    attribute: Option<&str>,
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let body = recv.to_lossy_string(ctx.heap_mut());
    let mut out = String::with_capacity(body.len() + tag.len() * 2 + 5);
    out.push('<');
    out.push_str(tag);
    if let Some(attr) = attribute {
        let raw = if let Some(arg) = args.first() {
            if arg.is_undefined() {
                "undefined".to_string()
            } else if let Some(s) = arg.as_string(ctx.heap_mut()) {
                s.to_lossy_string(ctx.heap_mut())
            } else {
                arg.display_string(ctx.heap())
            }
        } else {
            "undefined".to_string()
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
    Ok(Value::string(JsString::from_str(&out, ctx.heap_mut())?))
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
///
/// §22.1.3.10 `String.prototype.isWellFormed()`. Returns `true` if
/// every surrogate code unit is part of a valid pair.
fn impl_is_well_formed(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let units = recv.to_utf16_vec(ctx.heap_mut());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if (0xD800..=0xDBFF).contains(&u) {
            if i + 1 >= units.len() || !(0xDC00..=0xDFFF).contains(&units[i + 1]) {
                return Ok(Value::boolean(false));
            }
            i += 2;
        } else if (0xDC00..=0xDFFF).contains(&u) {
            return Ok(Value::boolean(false));
        } else {
            i += 1;
        }
    }
    Ok(Value::boolean(true))
}

/// §22.1.3.11 `String.prototype.toWellFormed()`. Replaces every
/// unpaired surrogate with `U+FFFD` (REPLACEMENT CHARACTER).
fn impl_to_well_formed(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let units = recv.to_utf16_vec(ctx.heap_mut());
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
    Ok(Value::string(JsString::from_utf16_units(
        &out,
        ctx.heap_mut(),
    )?))
}

fn impl_substr(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let size = recv.len() as i64;
    let raw_start = arg_int_or(ctx, args, 0, 0)?;
    let int_start = if raw_start == i64::MIN {
        0
    } else if raw_start < 0 {
        std::cmp::max(size + raw_start, 0)
    } else {
        std::cmp::min(raw_start, size)
    };
    let int_length = match args.get(1) {
        None => size,
        Some(v) if v.is_undefined() => size,
        Some(_) => {
            let raw = arg_int_or(ctx, args, 1, 0)?;
            std::cmp::min(std::cmp::max(raw, 0), size - int_start)
        }
    };
    if int_length <= 0 {
        return Ok(Value::string(JsString::empty(ctx.heap_mut())?));
    }
    Ok(Value::string(recv.slice(
        int_start as u32,
        int_length as u32,
        ctx.heap_mut(),
    )?))
}

fn impl_anchor(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "a", Some("name"))
}
fn impl_big(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "big", None)
}
fn impl_blink(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "blink", None)
}
fn impl_bold(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "b", None)
}
fn impl_fixed(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "tt", None)
}
fn impl_fontcolor(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "font", Some("color"))
}
fn impl_fontsize(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "font", Some("size"))
}
fn impl_italics(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "i", None)
}
fn impl_link(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "a", Some("href"))
}
fn impl_small(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "small", None)
}
fn impl_strike(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "strike", None)
}
fn impl_sub(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "sub", None)
}
fn impl_sup(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    create_html(ctx, receiver, args, "sup", None)
}

fn impl_trim_start(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    trim_impl(ctx, receiver, args, TrimSide::Start)
}

fn impl_trim_end(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    trim_impl(ctx, receiver, args, TrimSide::End)
}

fn impl_at(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let raw = arg_int_or(ctx, args, 0, 0)?;
    let len = recv.len() as i64;
    let idx = if raw < 0 {
        raw.saturating_add(len)
    } else {
        raw
    };
    if idx < 0 || idx >= len {
        return Ok(Value::undefined());
    }
    let unit = recv
        .char_code_at(idx as u32, ctx.heap_mut())
        .expect("index in range yields a code unit");
    Ok(Value::string(JsString::from_utf16_units(
        &[unit],
        ctx.heap_mut(),
    )?))
}

fn impl_code_point_at(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let raw = arg_int_or(ctx, args, 0, 0)?;
    let len = recv.len() as i64;
    if raw < 0 || raw >= len {
        return Ok(Value::undefined());
    }
    let idx = raw as u32;
    let cu1 = recv
        .char_code_at(idx, ctx.heap_mut())
        .expect("index in range");
    if (0xD800..=0xDBFF).contains(&cu1) && (idx + 1) < len as u32 {
        let cu2 = recv
            .char_code_at(idx + 1, ctx.heap_mut())
            .expect("idx+1 in range");
        if (0xDC00..=0xDFFF).contains(&cu2) {
            let cp = 0x10000u32 + ((u32::from(cu1) - 0xD800) << 10) + (u32::from(cu2) - 0xDC00);
            return Ok(Value::number(NumberValue::from_i32(cp as i32)));
        }
    }
    Ok(Value::number(NumberValue::from_i32(i32::from(cu1))))
}

/// Decode a UTF-16 unit slice to its code-point sequence. A lone
/// surrogate is preserved as its own scalar value so case mapping
/// round-trips it unchanged (§22.1.3.26 operates on code points).
fn decode_code_points(units: &[u16]) -> Vec<u32> {
    let mut cps = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if (0xD800..=0xDBFF).contains(&u)
            && let Some(&low) = units.get(i + 1)
            && (0xDC00..=0xDFFF).contains(&low)
        {
            let cp = 0x10000 + (((u as u32) - 0xD800) << 10) + ((low as u32) - 0xDC00);
            cps.push(cp);
            i += 2;
        } else {
            cps.push(u as u32);
            i += 1;
        }
    }
    cps
}

fn push_char_utf16(out: &mut Vec<u16>, ch: char) {
    let mut buf = [0u16; 2];
    out.extend_from_slice(ch.encode_utf16(&mut buf));
}

/// §11.4 Cased — a letter that has a case. Approximated by the
/// Uppercase / Lowercase derived properties exposed by `char`,
/// which covers the cased scalars the case-mapping tests exercise
/// (Latin, Greek, supplementary-plane mathematical letters).
fn is_cased(cp: u32) -> bool {
    char::from_u32(cp).is_some_and(|c| c.is_uppercase() || c.is_lowercase())
}

/// §11.4 Case_Ignorable — characters skipped over when testing the
/// Final_Sigma context: combining marks, format characters, modifier
/// letters/symbols, and the MidLetter/Single_Quote punctuation set.
fn is_case_ignorable(cp: u32) -> bool {
    matches!(cp,
        0x0027 | 0x002E | 0x003A | 0x00AD | 0x00B7 | 0x058A | 0x0387
        | 0x05F4 | 0x2018 | 0x2019 | 0x2024 | 0x2027 | 0x2060..=0x2064
        | 0xFE52 | 0xFE55 | 0xFF07 | 0xFF0E | 0xFF1A | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0xFEFF
        | 0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD
        | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF
        | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F
        // Supplementary-plane non-spacing marks (musical / variation).
        | 0x1D167..=0x1D169 | 0x1D17B..=0x1D182 | 0x1D185..=0x1D18B
        | 0x1D1AA..=0x1D1AD | 0x1D242..=0x1D244 | 0xE0100..=0xE01EF
    ) || char::from_u32(cp).is_some_and(|c| {
        // Lm (modifier letter) heuristic via the common ranges.
        matches!(c as u32, 0x02B0..=0x02FF | 0x1D2C..=0x1D6A | 0x1DA0..=0x1DBF)
    })
}

/// §22.1.3.26 Final_Sigma context: the GREEK CAPITAL LETTER SIGMA at
/// `idx` is preceded by a cased scalar and not followed by one,
/// skipping Case_Ignorable scalars in both directions.
fn sigma_is_final(cps: &[u32], idx: usize) -> bool {
    let before = (0..idx).rev().find(|&j| !is_case_ignorable(cps[j]));
    let preceded = before.is_some_and(|j| is_cased(cps[j]));
    let after = ((idx + 1)..cps.len()).find(|&j| !is_case_ignorable(cps[j]));
    let followed = after.is_some_and(|j| is_cased(cps[j]));
    preceded && !followed
}

/// §22.1.3.{26,28} `toUnicodeLowercase` / `toUnicodeUppercase` over
/// code points via the Unicode default case mappings (`char`'s
/// `to_lowercase` / `to_uppercase`, which include the unconditional
/// SpecialCasing 1→N expansions such as `ß`→`SS`), plus the
/// conditional Final_Sigma lowercase rule.
fn unicode_case_map(units: &[u16], upper: bool) -> Vec<u16> {
    let cps = decode_code_points(units);
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    for (i, &cp) in cps.iter().enumerate() {
        let Some(ch) = char::from_u32(cp) else {
            out.push(cp as u16);
            continue;
        };
        if !upper && cp == 0x03A3 {
            out.push(if sigma_is_final(&cps, i) {
                0x03C2
            } else {
                0x03C3
            });
            continue;
        }
        if upper {
            ch.to_uppercase().for_each(|m| push_char_utf16(&mut out, m));
        } else {
            ch.to_lowercase().for_each(|m| push_char_utf16(&mut out, m));
        }
    }
    out
}

fn impl_to_lower_case(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let units = recv.to_utf16_vec(ctx.heap_mut());
    let lowered = unicode_case_map(&units, false);
    Ok(Value::string(JsString::from_utf16_units(
        &lowered,
        ctx.heap_mut(),
    )?))
}

fn impl_to_upper_case(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let units = recv.to_utf16_vec(ctx.heap_mut());
    let upper = unicode_case_map(&units, true);
    Ok(Value::string(JsString::from_utf16_units(
        &upper,
        ctx.heap_mut(),
    )?))
}

/// Map a [`crate::VmError`] from an interpreter re-entry onto the
/// native error surface, preserving thrown user values.
fn vm_err(err: crate::VmError, name: &'static str) -> NativeError {
    match err {
        crate::VmError::Uncaught { value } => NativeError::Thrown {
            name,
            message: value,
        },
        crate::VmError::TypeError { message } => NativeError::TypeError {
            name,
            reason: message,
        },
        crate::VmError::RangeError { message } => NativeError::RangeError {
            name,
            reason: message,
        },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}

/// `? Get(value, key)` honouring accessor getters; used for the
/// `@@`-symbol method probe and `flags` read.
fn get_value(
    ctx: &mut NativeCtx<'_>,
    value: Value,
    key: &VmPropertyKey,
    name: &'static str,
) -> Result<Value, NativeError> {
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error(name, "missing execution context"))?;
    let interp = ctx.interp_mut();
    match interp
        .ordinary_get_value(&exec, value, value, key, 0)
        .map_err(|e| vm_err(e, name))?
    {
        VmGetOutcome::Value(v) => Ok(v),
        VmGetOutcome::InvokeGetter { getter } => interp
            .run_callable_sync(&exec, &getter, value, SmallVec::new())
            .map_err(|e| vm_err(e, name)),
    }
}

/// §7.3.11 GetMethod for a well-known symbol: returns `None` when the
/// property is `undefined`/`null`, errors when present but not
/// callable.
fn get_symbol_method(
    ctx: &mut NativeCtx<'_>,
    value: Value,
    wk: WellKnown,
    name: &'static str,
) -> Result<Option<Value>, NativeError> {
    let sym = ctx.interp_mut().well_known_symbols().get(wk);
    let method = get_value(ctx, value, &VmPropertyKey::Symbol(sym), name)?;
    if method.is_nullish() {
        return Ok(None);
    }
    if !method.is_callable() {
        return Err(type_error(name, "method is not callable"));
    }
    Ok(Some(method))
}

/// §7.2.8 IsRegExp — `@@match` (if defined) decides; otherwise the
/// native `[[RegExpMatcher]]` brand.
fn is_reg_exp(
    ctx: &mut NativeCtx<'_>,
    value: Value,
    name: &'static str,
) -> Result<bool, NativeError> {
    if !value.is_object_type() {
        return Ok(false);
    }
    let sym = ctx.interp_mut().well_known_symbols().get(WellKnown::Match);
    let matcher = get_value(ctx, value, &VmPropertyKey::Symbol(sym), name)?;
    if !matcher.is_undefined() {
        return Ok(matcher.to_boolean(ctx.heap()));
    }
    Ok(value.as_regexp().is_some())
}

/// Coerce a single value with the §7.1.17 `ToString` ladder (user
/// `toString` / `valueOf` / `@@toPrimitive` observable; objects route
/// through ToPrimitive). User exceptions propagate verbatim.
fn value_to_string(
    ctx: &mut NativeCtx<'_>,
    value: Value,
    name: &'static str,
) -> Result<JsString, NativeError> {
    if let Some(s) = value.as_string(ctx.heap()) {
        return Ok(s);
    }
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error(name, "missing execution context"))?;
    let text = ctx
        .interp_mut()
        .coerce_to_string(&exec, &value)
        .map_err(|e| vm_err(e, name))?;
    JsString::from_str(&text, ctx.heap_mut()).map_err(NativeError::from)
}

/// §22.1.3.17.1 GetSubstitution over UTF-16 units for a string-search
/// replace (no capture groups, so `$n` / `$<name>` stay literal).
/// Honours `$$`, `$&`, `` $` ``, and `$'`.
fn get_substitution(
    matched: &[u16],
    string: &[u16],
    position: usize,
    template: &[u16],
) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(template.len());
    let mut i = 0;
    while i < template.len() {
        let c = template[i];
        if c != b'$' as u16 || i + 1 >= template.len() {
            out.push(c);
            i += 1;
            continue;
        }
        match template[i + 1] {
            0x24 => {
                out.push(0x24);
                i += 2;
            } // $$
            0x26 => {
                out.extend_from_slice(matched);
                i += 2;
            } // $&
            0x60 => {
                out.extend_from_slice(&string[..position.min(string.len())]);
                i += 2;
            } // $`
            0x27 => {
                let tail = (position + matched.len()).min(string.len());
                out.extend_from_slice(&string[tail..]);
                i += 2;
            } // $'
            _ => {
                out.push(c);
                i += 1;
            } // literal `$`
        }
    }
    out
}

/// §22.1.3.19 `String.prototype.replace` / §22.1.3.20 `replaceAll`,
/// full spec ladder: `@@replace` dispatch, `ToString` coercion of
/// receiver / searchValue, functional and `$`-template substitution.
fn string_replace_spec(
    replace_all: bool,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let name: &'static str = if replace_all { "replaceAll" } else { "replace" };
    let this = *ctx.this_value();
    if this.is_nullish() {
        return Err(type_error(name, "called on null or undefined"));
    }
    let search = args.first().copied().unwrap_or_else(Value::undefined);
    let replace_value = args.get(1).copied().unwrap_or_else(Value::undefined);

    // §22.1.3.19 step 2 — only an Object searchValue is probed for
    // `@@replace`; a primitive never has its `@@replace` accessed.
    if search.is_object_type() {
        if replace_all && is_reg_exp(ctx, search, name)? {
            let flags = get_value(ctx, search, &VmPropertyKey::String("flags"), name)?;
            if flags.is_nullish() {
                return Err(type_error(name, "flags is null or undefined"));
            }
            let flags_str = value_to_string(ctx, flags, name)?;
            if !flags_str.to_lossy_string(ctx.heap()).contains('g') {
                return Err(type_error(
                    name,
                    "replaceAll must be called with a global RegExp",
                ));
            }
        }
        if let Some(method) = get_symbol_method(ctx, search, WellKnown::Replace, name)? {
            let exec = ctx
                .execution_context()
                .cloned()
                .ok_or_else(|| type_error(name, "missing execution context"))?;
            let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![this, replace_value];
            return ctx
                .interp_mut()
                .run_callable_sync(&exec, &method, search, cb_args)
                .map_err(|e| vm_err(e, name));
        }
    }

    // String search path.
    let string = value_to_string(ctx, this, name)?;
    let search_str = value_to_string(ctx, search, name)?;
    let functional = replace_value.is_callable();
    let replace_template = if functional {
        None
    } else {
        Some(value_to_string(ctx, replace_value, name)?)
    };

    let string_units = string.to_utf16_vec(ctx.heap());
    let search_units = search_str.to_utf16_vec(ctx.heap());
    let template_units = replace_template
        .as_ref()
        .map(|s| s.to_utf16_vec(ctx.heap()));

    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error(name, "missing execution context"))?;

    let search_len = search_units.len();
    let mut out: Vec<u16> = Vec::with_capacity(string_units.len());
    let mut cursor = 0usize;
    let mut position = find_substr(&string_units, &search_units, 0);
    while let Some(pos) = position {
        out.extend_from_slice(&string_units[cursor..pos]);
        let replacement = if functional {
            let matched = JsString::from_utf16_units(&search_units, ctx.heap_mut())?;
            let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![
                Value::string(matched),
                Value::number_f64(pos as f64),
                Value::string(string),
            ];
            let raw = ctx
                .interp_mut()
                .run_callable_sync(&exec, &replace_value, Value::undefined(), cb_args)
                .map_err(|e| vm_err(e, name))?;
            value_to_string(ctx, raw, name)?.to_utf16_vec(ctx.heap())
        } else {
            get_substitution(
                &search_units,
                &string_units,
                pos,
                template_units
                    .as_ref()
                    .expect("non-functional has template"),
            )
        };
        out.extend_from_slice(&replacement);
        cursor = pos + search_len;
        if !replace_all {
            break;
        }
        // Advance by at least one unit for an empty search string so an
        // empty match cannot loop forever.
        let next_from = if search_len == 0 { pos + 1 } else { cursor };
        if next_from > string_units.len() {
            break;
        }
        if search_len == 0 && pos < string_units.len() {
            out.push(string_units[pos]);
            cursor = next_from;
        }
        position = find_substr(&string_units, &search_units, next_from);
    }
    out.extend_from_slice(&string_units[cursor.min(string_units.len())..]);
    Ok(Value::string(JsString::from_utf16_units(
        &out,
        ctx.heap_mut(),
    )?))
}

fn impl_replace(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    if let Some(re) = args.first().and_then(|v| v.as_regexp()) {
        let replacement = arg_to_string(ctx, args, 1)?;
        let replacement_units = replacement.to_utf16_vec(ctx.heap_mut());
        return regex_replace(recv, &re, ctx.heap_mut(), &replacement_units);
    }
    let needle = arg_to_string(ctx, args, 0)?;
    let replacement = arg_to_string(ctx, args, 1)?;
    let recv_units = recv.to_utf16_vec(ctx.heap_mut());
    let needle_units = needle.to_utf16_vec(ctx.heap_mut());
    let replacement_units = replacement.to_utf16_vec(ctx.heap_mut());

    if needle_units.is_empty() {
        let mut buf = Vec::with_capacity(recv_units.len() + replacement_units.len());
        buf.extend_from_slice(&replacement_units);
        buf.extend_from_slice(&recv_units);
        return Ok(Value::string(JsString::from_utf16_units(
            &buf,
            ctx.heap_mut(),
        )?));
    }
    let pos = match find_substr(&recv_units, &needle_units, 0) {
        Some(p) => p,
        None => return Ok(Value::string(recv)),
    };
    let mut buf =
        Vec::with_capacity(recv_units.len() - needle_units.len() + replacement_units.len());
    buf.extend_from_slice(&recv_units[..pos]);
    buf.extend_from_slice(&replacement_units);
    buf.extend_from_slice(&recv_units[pos + needle_units.len()..]);
    Ok(Value::string(JsString::from_utf16_units(
        &buf,
        ctx.heap_mut(),
    )?))
}

fn impl_replace_all(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    if let Some(re) = args.first().and_then(|v| v.as_regexp()) {
        // Spec: `replaceAll` requires the `g` flag for regex args.
        let heap = ctx.heap();
        if !re.flags(heap).global {
            return Err(type_error(
                "String.prototype",
                "must be a global regular expression",
            ));
        }
        let replacement = arg_to_string(ctx, args, 1)?;
        let replacement_units = replacement.to_utf16_vec(ctx.heap_mut());
        return regex_replace(recv, &re, ctx.heap_mut(), &replacement_units);
    }
    let needle = arg_to_string(ctx, args, 0)?;
    let replacement = arg_to_string(ctx, args, 1)?;
    let recv_units = recv.to_utf16_vec(ctx.heap_mut());
    let needle_units = needle.to_utf16_vec(ctx.heap_mut());
    let replacement_units = replacement.to_utf16_vec(ctx.heap_mut());

    if needle_units.is_empty() {
        // Spec: insert replacement before each unit and at the end.
        let mut buf =
            Vec::with_capacity(recv_units.len() + replacement_units.len() * (recv_units.len() + 1));
        for &u in &recv_units {
            buf.extend_from_slice(&replacement_units);
            buf.push(u);
        }
        buf.extend_from_slice(&replacement_units);
        return Ok(Value::string(JsString::from_utf16_units(
            &buf,
            ctx.heap_mut(),
        )?));
    }
    if recv_units.len() < needle_units.len() {
        return Ok(Value::string(recv));
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
    Ok(Value::string(JsString::from_utf16_units(
        &buf,
        ctx.heap_mut(),
    )?))
}

fn impl_split(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;

    // Regex separator → defer to the dedicated walker.
    if let Some(re) = args.first().and_then(|v| v.as_regexp()) {
        let limit = parse_split_limit(ctx, args)?;
        return regex_split(ctx, recv, &re, limit);
    }

    // §22.1.3.21 step 6 — lim = ToUint32(limit), coerced BEFORE the
    // separator's ToString (step 7) per spec operand order.
    let limit = parse_split_limit(ctx, args)?;
    // step 7 — R = ToString(separator). A missing / `undefined`
    // separator has no coercion side effect; every other operand runs
    // the ToString ladder here.
    let separator_owned: JsString;
    let separator: Option<JsString> = match args.first() {
        None => None,
        Some(v) if v.is_undefined() => None,
        Some(v) => Some(if let Some(s) = v.as_string(ctx.heap_mut()) {
            s
        } else {
            separator_owned = arg_to_string(ctx, args, 0)?;
            separator_owned
        }),
    };
    // step 8 — `lim = 0` returns an empty array, ahead of the
    // `undefined` separator whole-string case (step 9).
    if limit == 0 {
        return Ok(Value::array(ctx.array_from_elements_with_roots(
            std::iter::empty(),
            &[],
            &[],
        )?));
    }
    // step 9 — a `undefined` / missing separator yields `[S]`.
    let Some(separator) = separator else {
        let singleton = [Value::string(recv)];
        return Ok(Value::array(ctx.array_from_elements_with_roots(
            singleton.iter().cloned(),
            &[],
            &[singleton.as_slice()],
        )?));
    };

    let recv_units = recv.to_utf16_vec(ctx.heap_mut());
    let sep_units = separator.to_utf16_vec(ctx.heap_mut());

    // Empty separator: split into individual code units (capped).
    if sep_units.is_empty() {
        let mut out: Vec<Value> = Vec::with_capacity((limit as usize).min(recv_units.len()));
        for &u in recv_units.iter().take(limit as usize) {
            out.push(Value::string(JsString::from_utf16_units(
                &[u],
                ctx.heap_mut(),
            )?));
        }
        return Ok(Value::array(ctx.array_from_elements_with_roots(
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
                let part = JsString::from_utf16_units(&recv_units[start..pos], ctx.heap_mut())?;
                out.push(Value::string(part));
                start = pos + sep_units.len();
            }
            None => break,
        }
    }
    if (out.len() as u32) < limit {
        let part = JsString::from_utf16_units(&recv_units[start..], ctx.heap_mut())?;
        out.push(Value::string(part));
    }
    Ok(Value::array(ctx.array_from_elements_with_roots(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// Common limit-arg parser shared by string-separator and
/// regex-separator `split` paths.
fn parse_split_limit(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<u32, NativeError> {
    // §22.1.3.23 step 6: `limit` defaults to 2^32 - 1 and is
    // ToUint32-coerced. Foundation accepts the spec set
    // (`Number` / `Boolean` / `null` / `String` — strings parsed as
    // decimal integers). Non-integer / negative coerce to 0 per
    // ToUint32 modulo.
    let Some(arg) = args.get(1) else {
        return Ok(u32::MAX);
    };
    if arg.is_undefined() {
        return Ok(u32::MAX);
    }
    if let Some(n) = arg.as_number() {
        // §7.1.6 ToUint32 — NaN / ±∞ → +0, otherwise the truncated
        // magnitude taken modulo 2^32 (so `2**32` wraps to 0 rather
        // than clamping to the maximum).
        let f = n.as_f64();
        let u = if !f.is_finite() {
            0
        } else {
            f.trunc().rem_euclid(4_294_967_296.0) as u32
        };
        return Ok(u);
    }
    if let Some(b) = arg.as_boolean() {
        return Ok(if b { 1 } else { 0 });
    }
    if arg.is_null() {
        return Ok(0);
    }
    if let Some(s) = arg.as_string(ctx.heap_mut()) {
        let text = s.to_lossy_string(ctx.heap_mut());
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(0);
        }
        return Ok(trimmed.parse::<i64>().map_or(0, |v| {
            if v < 0 {
                0
            } else if v > u32::MAX as i64 {
                u32::MAX
            } else {
                v as u32
            }
        }));
    }
    Err(type_error("String.prototype", "must be a number"))
}

fn regex_replace(
    recv: JsString,
    re: &JsRegExp,
    gc_heap: &mut otter_gc::GcHeap,
    replacement_template: &[u16],
) -> Result<Value, NativeError> {
    let recv_units = recv.to_utf16_vec(gc_heap);
    let matches = collect_regex_matches(re, gc_heap, &recv_units);
    if matches.is_empty() {
        return Ok(Value::string(recv));
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
    Ok(Value::string(JsString::from_utf16_units(&buf, gc_heap)?))
}

fn regex_split(
    ctx: &mut NativeCtx<'_>,
    recv: JsString,
    re: &JsRegExp,
    limit: u32,
) -> Result<Value, NativeError> {
    if limit == 0 {
        return Ok(Value::array(ctx.array_from_elements_with_roots(
            std::iter::empty(),
            &[],
            &[],
        )?));
    }
    let recv_units = recv.to_utf16_vec(ctx.heap_mut());
    let mut out: Vec<Value> = Vec::new();
    let mut cursor: usize = 0;
    let mut iter = re.find_from_utf16(ctx.heap(), &recv_units, 0).into_iter();
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
                .find_from_utf16(ctx.heap(), &recv_units, cursor)
                .into_iter();
            continue;
        }
        let part = JsString::from_utf16_units(&recv_units[cursor..m.range.start], ctx.heap_mut())?;
        out.push(Value::string(part));
        cursor = m.range.end;
    }
    if (out.len() as u32) < limit {
        let part = JsString::from_utf16_units(&recv_units[cursor..], ctx.heap_mut())?;
        out.push(Value::string(part));
    }
    Ok(Value::array(ctx.array_from_elements_with_roots(
        out.iter().cloned(),
        &[],
        &[out.as_slice()],
    )?))
}

/// §22.1.3.13 step 6 / §22.1.3.14 step 6 / §22.1.3.15 step 4: when
/// the first argument is not a `RegExp`, ToString-coerce it and run
/// `RegExpCreate(pattern, flags)`. The string fast-path matters for
/// idiomatic JS like `"foo".match("o+")` and `"foo".matchAll("o")`.
fn coerce_pattern_to_regexp(
    value: &Value,
    flags: &str,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<JsRegExp, NativeError> {
    // §22.1.3.{13,14,15} step 6 — `pattern = ? ToString(arg)`.
    // Coerce every spec-relevant operand before compiling.
    let pattern_units: Vec<u16> = if value.is_undefined() {
        Vec::new()
    } else if let Some(s) = value.as_string(gc_heap) {
        s.to_utf16_vec(gc_heap)
    } else if value.is_null() {
        "null".encode_utf16().collect()
    } else if let Some(b) = value.as_boolean() {
        if b { "true" } else { "false" }.encode_utf16().collect()
    } else if let Some(n) = value.as_number() {
        n.to_display_string().encode_utf16().collect()
    } else if let Some(b) = value.as_big_int() {
        b.to_decimal_string(&*gc_heap).encode_utf16().collect()
    } else if let Some(r) = value.as_regexp() {
        // RegExpCreate(R, flags) with a RegExp source reuses its pattern
        // (the requested flags win), so `matchAll`'s synthesised matcher
        // sweeps the same pattern with `g` set.
        r.pattern_utf16(gc_heap)
    } else if let Some(obj) = value.as_object() {
        let gc = &*gc_heap;
        if let Some(s) = crate::object::string_data(obj, gc) {
            s.to_utf16_vec(gc_heap)
        } else if let Some(b) = crate::object::boolean_data(obj, gc) {
            if b { "true" } else { "false" }.encode_utf16().collect()
        } else if let Some(n) = crate::object::number_data(obj, gc) {
            n.to_display_string().encode_utf16().collect()
        } else {
            "[object Object]".encode_utf16().collect()
        }
    } else {
        return Err(type_error(
            "String.prototype",
            "must be a regular expression or string",
        ));
    };
    JsRegExp::compile(gc_heap, &pattern_units, flags).map_err(|_| {
        type_error(
            "String.prototype",
            "is not a valid regular expression pattern",
        )
    })
}

fn impl_match(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let undef = Value::undefined();
    let re = if let Some(r) = args.first().and_then(|v| v.as_regexp()) {
        r
    } else {
        let arg0 = args.first().unwrap_or(&undef);
        coerce_pattern_to_regexp(arg0, "", ctx.heap_mut())?
    };
    let re = &re;
    let recv_units = recv.to_utf16_vec(ctx.heap_mut());
    if re.flags(ctx.heap()).global {
        // `g` flag → return array of full matches only (no captures).
        let matches = collect_regex_matches(re, ctx.heap(), &recv_units);
        if matches.is_empty() {
            return Ok(Value::null());
        }
        let mut out: Vec<Value> = Vec::with_capacity(matches.len());
        for m in &matches {
            let s = JsString::from_utf16_units(&recv_units[m.range.clone()], ctx.heap_mut())?;
            out.push(Value::string(s));
        }
        return Ok(Value::array(ctx.array_from_elements_with_roots(
            out.iter().cloned(),
            &[],
            &[out.as_slice()],
        )?));
    }
    // Non-global → mirror `RegExp.prototype.exec` (carries
    // `index` / `input` / `groups` per §22.2.7.2).
    let m = match re
        .find_from_utf16(ctx.heap(), &recv_units, 0)
        .into_iter()
        .next()
    {
        Some(m) => m,
        None => return Ok(Value::null()),
    };
    let recv_clone = recv;
    let has_indices = re.flags(ctx.heap()).has_indices;
    let arr = crate::regexp_prototype::build_match_result_native(
        &m,
        &recv_units,
        recv_clone,
        has_indices,
        ctx,
        &[],
        &[],
    )
    .map_err(|err| type_error("String.prototype", err.to_string()))?;
    Ok(Value::array(arr))
}

fn impl_match_all(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    // The receiver is already `S = ToString(O)` (coerced uniformly by
    // `native_string_method`), and `native_string_method`'s symbol-method
    // block has already handled §22.1.3.14 steps 2.a-d — the `@@matchAll`
    // delegation and the global-flag check for a non-nullish RegExp arg.
    // What reaches here is the fallback: steps 4-5 build a fresh global
    // matcher and `Invoke(rx, @@matchAll, « S »)`, so a user-overridden
    // `RegExp.prototype[@@matchAll]` is observed instead of an inlined
    // iterator.
    const NAME: &str = "String.prototype.matchAll";
    let s = receiver_string(ctx, receiver)?;
    let arg = args.first().copied().unwrap_or_else(Value::undefined);
    let rx = coerce_pattern_to_regexp(&arg, "g", ctx.heap_mut())?;
    let rx_value = Value::regexp(rx);
    let method = get_symbol_method(ctx, rx_value, WellKnown::MatchAll, NAME)?
        .ok_or_else(|| type_error(NAME, "RegExp has no @@matchAll method"))?;
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| type_error(NAME, "missing execution context"))?;
    let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![Value::string(s)];
    ctx.interp_mut()
        .run_callable_sync(&exec, &method, rx_value, cb_args)
        .map_err(|e| vm_err(e, NAME))
}

fn impl_search(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let undef = Value::undefined();
    let re = if let Some(r) = args.first().and_then(|v| v.as_regexp()) {
        r
    } else {
        let arg0 = args.first().unwrap_or(&undef);
        coerce_pattern_to_regexp(arg0, "", ctx.heap_mut())?
    };
    let re = &re;
    let recv_units = recv.to_utf16_vec(ctx.heap_mut());
    // `search` always starts at index 0 — `lastIndex` is ignored
    // and not mutated per spec §22.1.3.13.
    let heap = ctx.heap();
    let pos = re
        .find_from_utf16(heap, &recv_units, 0)
        .into_iter()
        .next()
        .map_or(-1, |m| m.range.start as i32);
    Ok(Value::number(NumberValue::from_i32(pos)))
}

type StringNativeFn = fn(&mut NativeCtx<'_>, &Value, &[Value]) -> Result<Value, NativeError>;

fn intrinsic_impl(name: &str) -> Option<StringNativeFn> {
    Some(match name {
        "length" => impl_length,
        "charCodeAt" => impl_char_code_at,
        "charAt" => impl_char_at,
        "codePointAt" => impl_code_point_at,
        "at" => impl_at,
        "slice" => impl_slice,
        "substring" => impl_substring,
        "indexOf" => impl_index_of,
        "lastIndexOf" => impl_last_index_of,
        "includes" => impl_includes,
        "startsWith" => impl_starts_with,
        "endsWith" => impl_ends_with,
        "concat" => impl_concat,
        "repeat" => impl_repeat,
        "padStart" => impl_pad_start,
        "padEnd" => impl_pad_end,
        "trim" => impl_trim,
        "trimStart" | "trimLeft" => impl_trim_start,
        "trimEnd" | "trimRight" => impl_trim_end,
        "isWellFormed" => impl_is_well_formed,
        "toWellFormed" => impl_to_well_formed,
        "substr" => impl_substr,
        "anchor" => impl_anchor,
        "big" => impl_big,
        "blink" => impl_blink,
        "bold" => impl_bold,
        "fixed" => impl_fixed,
        "fontcolor" => impl_fontcolor,
        "fontsize" => impl_fontsize,
        "italics" => impl_italics,
        "link" => impl_link,
        "small" => impl_small,
        "strike" => impl_strike,
        "sub" => impl_sub,
        "sup" => impl_sup,
        "toLowerCase" | "toLocaleLowerCase" => impl_to_lower_case,
        "toUpperCase" | "toLocaleUpperCase" => impl_to_upper_case,
        "replace" => impl_replace,
        "replaceAll" => impl_replace_all,
        "split" => impl_split,
        "match" => impl_match,
        "matchAll" => impl_match_all,
        "search" => impl_search,
        "localeCompare" => impl_locale_compare,
        "normalize" => impl_normalize,
        "toString" | "valueOf" => impl_to_string,
        _ => return None,
    })
}

/// §22.1.3.10 String.prototype.lastIndexOf(search, fromIndex?).
fn impl_last_index_of(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let needle = arg_to_string(ctx, args, 0)?;
    // §22.1.3.9 step 5 — `numPos = ToNumber(position)`; if it is NaN the
    // search position is +∞ (the whole string), otherwise
    // ToIntegerOrInfinity clamped to `[0, len]`. `position` is already
    // ToNumber-coerced by the shared arg pre-coercion, so a NaN here must
    // map to the end, not to 0.
    let len = recv.len();
    let position = match args.get(1) {
        Some(v) if !v.is_undefined() => {
            let n = v.as_number().map(|x| x.as_f64()).unwrap_or(f64::NAN);
            if n.is_nan() || n >= len as f64 {
                len
            } else if n <= 0.0 {
                0
            } else {
                n as u32
            }
        }
        _ => len,
    };
    let pos = recv
        .last_index_of(needle, position, None, ctx.heap_mut())
        .map_err(|Interrupted| type_error("String.prototype", "interrupted"))?;
    let value = match pos {
        Some(p) => NumberValue::from_i32(p as i32),
        None => NumberValue::from_i32(-1),
    };
    Ok(Value::number(value))
}

/// §22.1.3.12 String.prototype.localeCompare. Foundation falls
/// back to spec-default Unicode code-point comparison; locale-
/// aware ordering ships through `Intl.Collator`.
fn impl_locale_compare(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?.to_lossy_string(ctx.heap_mut());
    let other = match args.first() {
        Some(v) => {
            if let Some(s) = v.as_string(ctx.heap_mut()) {
                s.to_lossy_string(ctx.heap_mut())
            } else {
                v.display_string(ctx.heap())
            }
        }
        None => "undefined".to_string(),
    };
    let cmp = match recv.cmp(&other) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Value::number(crate::number::NumberValue::from_i32(cmp)))
}

/// §22.1.3.13 String.prototype.normalize(form?). Foundation accepts
/// `"NFC"` / `"NFD"` / `"NFKC"` / `"NFKD"` (default `"NFC"`) and
/// returns the receiver string itself — the foundation slice does
/// not perform full Unicode normalisation but mirrors the spec
/// surface so call sites that depend on the method existing keep
/// working. Real normalisation lands alongside the ICU integration
/// follow-up.
fn impl_normalize(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    let form = match args.first() {
        None => "NFC".to_string(),
        Some(v) if v.is_undefined() => "NFC".to_string(),
        Some(v) => {
            if let Some(s) = v.as_string(ctx.heap_mut()) {
                s.to_lossy_string(ctx.heap_mut())
            } else {
                return Err(type_error("String.prototype", "must be a string"));
            }
        }
    };
    if !matches!(form.as_str(), "NFC" | "NFD" | "NFKC" | "NFKD") {
        return Err(type_error(
            "String.prototype",
            "must be one of NFC / NFD / NFKC / NFKD",
        ));
    }
    Ok(Value::string(recv))
}

/// §22.1.3.27 String.prototype.toString — returns the primitive.
fn impl_to_string(
    ctx: &mut NativeCtx<'_>,
    receiver: &Value,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let recv = receiver_string(ctx, receiver)?;
    Ok(Value::string(recv))
}

/// Whether `name` is installed on `String.prototype`.
#[must_use]
pub fn is_builtin_method(name: &str) -> bool {
    STRING_PROTOTYPE_METHODS.iter().any(|m| m.name == name)
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
/// looks up the shared implementation by name.
fn native_string_method(
    name: &'static str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    // §22.1.3.19 `replace` / §22.1.3.20 `replaceAll` run the full spec
    // ladder (`@@replace` dispatch, `ToString` coercion, functional and
    // `$`-template substitution), which needs the interpreter context.
    if name == "replace" || name == "replaceAll" {
        return string_replace_spec(name == "replaceAll", ctx, args);
    }
    // §22.1.3.23 `split` — an Object separator delegates to `@@split`
    // (RegExp and user objects) with the un-stringified receiver.
    if name == "split" {
        let this = *ctx.this_value();
        if this.is_nullish() {
            return Err(type_error("split", "called on null or undefined"));
        }
        let separator = args.first().copied().unwrap_or_else(Value::undefined);
        if separator.is_object_type()
            && let Some(splitter) = get_symbol_method(ctx, separator, WellKnown::Split, "split")?
        {
            let exec = ctx
                .execution_context()
                .cloned()
                .ok_or_else(|| type_error("split", "missing execution context"))?;
            let limit = args.get(1).copied().unwrap_or_else(Value::undefined);
            let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![this, limit];
            return ctx
                .interp_mut()
                .run_callable_sync(&exec, &splitter, separator, cb_args)
                .map_err(|e| vm_err(e, "split"));
        }
    }
    // §22.1.3.{11,13,14} `match` / `search` / `matchAll` — an Object
    // argument delegates to `@@match` / `@@search` / `@@matchAll`.
    if let Some(wk) = match name {
        "match" => Some(WellKnown::Match),
        "search" => Some(WellKnown::Search),
        "matchAll" => Some(WellKnown::MatchAll),
        _ => None,
    } {
        let this = *ctx.this_value();
        if this.is_nullish() {
            return Err(type_error(name, "called on null or undefined"));
        }
        let arg = args.first().copied().unwrap_or_else(Value::undefined);
        if arg.is_object_type() {
            // §22.1.3.14 step 5.b — `matchAll` requires a global RegExp.
            if name == "matchAll" && is_reg_exp(ctx, arg, name)? {
                let flags = get_value(ctx, arg, &VmPropertyKey::String("flags"), name)?;
                if flags.is_nullish() {
                    return Err(type_error(name, "flags is null or undefined"));
                }
                let flags_str = value_to_string(ctx, flags, name)?;
                if !flags_str.to_lossy_string(ctx.heap()).contains('g') {
                    return Err(type_error(name, "matchAll must use a global RegExp"));
                }
            }
            if let Some(method) = get_symbol_method(ctx, arg, wk, name)? {
                let exec = ctx
                    .execution_context()
                    .cloned()
                    .ok_or_else(|| type_error(name, "missing execution context"))?;
                let cb_args: SmallVec<[Value; 8]> = smallvec::smallvec![this];
                return ctx
                    .interp_mut()
                    .run_callable_sync(&exec, &method, arg, cb_args)
                    .map_err(|e| vm_err(e, name));
            }
        }
    }
    let receiver = *ctx.this_value();
    // §22.1.3 — every `String.prototype` method (other than
    // `toString` / `valueOf`, which use `thisStringValue`) opens with
    // `RequireObjectCoercible(this)` then `S = ? ToString(this)`. The
    // old receiver-only helper inspected internal slots, so a
    // non-wrapper Object receiver with a user `toString` (or one that
    // throws / returns a Symbol) silently fell back to
    // `"[object Object]"`. Coerce the receiver here, uniformly, so the
    // spec `ToString` ladder fires for `.call` / `.apply` on any
    // receiver and user `toString` / `@@toPrimitive` / abrupt
    // completions are observed.
    let coerce_receiver = !matches!(name, "toString" | "valueOf");
    let receiver = if coerce_receiver {
        let needs_coerce = receiver.is_object()
            || receiver.is_array()
            || receiver.is_function()
            || receiver.is_closure()
            || receiver.is_native_function()
            || receiver.is_bound_function()
            || receiver.is_class_constructor()
            || receiver.is_proxy()
            || receiver.is_regexp()
            || receiver.is_promise()
            || receiver.is_map()
            || receiver.is_set();
        if receiver.is_nullish() {
            return Err(NativeError::TypeError {
                name,
                reason: "Cannot convert undefined or null to object".to_string(),
            });
        }
        if needs_coerce && let Some(exec) = ctx.execution_context().cloned() {
            let interp = ctx.interp_mut();
            let s = interp
                .coerce_to_string(&exec, &receiver)
                .map_err(|e| match e {
                    crate::VmError::Uncaught { value } => NativeError::Thrown {
                        name,
                        message: value,
                    },
                    crate::VmError::TypeError { message } => NativeError::TypeError {
                        name,
                        reason: message,
                    },
                    other => NativeError::TypeError {
                        name,
                        reason: other.to_string(),
                    },
                })?;

            Value::string(JsString::from_str(&s, ctx.heap_mut()).map_err(|_| {
                NativeError::TypeError {
                    name,
                    reason: "out of memory".to_string(),
                }
            })?)
        } else {
            receiver
        }
    } else {
        receiver
    };
    // §22.1.3.* String.prototype.* int / string argument coercion —
    // run the SAME shared routine as the `Op::CallMethodValue` String
    // arm (`Interpreter::coerce_string_method_args`) so `.call(...)` /
    // `.apply(...)` and `"s".m(...)` coerce identically (index-like
    // operands via full `ToNumber`, string operands via
    // `ToPrimitive(String)`), observing user `@@toPrimitive` /
    // `valueOf` / `toString`.
    let mut coerced_args: smallvec::SmallVec<[Value; 4]> = args.iter().cloned().collect();
    if let Some(exec) = ctx.execution_context().cloned() {
        ctx.interp_mut()
            .coerce_string_method_args(&exec, name, &mut coerced_args)
            .map_err(|e| crate::native_function::vm_to_native_error(e, name))?;
    }
    let impl_fn = intrinsic_impl(name).ok_or_else(|| NativeError::TypeError {
        name,
        reason: "unknown String.prototype method".to_string(),
    })?;
    impl_fn(ctx, &receiver, &coerced_args)
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
        /// through this table; direct calls resolve through the same
        /// native functions via `GetMethod + Call`.
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
    bridge_index_of        => "indexOf",        1;
    bridge_last_index_of   => "lastIndexOf",    1;
    bridge_includes        => "includes",       1;
    bridge_starts_with     => "startsWith",     1;
    bridge_ends_with       => "endsWith",       1;
    bridge_concat          => "concat",         1;
    bridge_repeat          => "repeat",         1;
    bridge_pad_start       => "padStart",       1;
    bridge_pad_end         => "padEnd",         1;
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
    use crate::{Interpreter, NativeCallInfo};

    /// Drive a builtin string method with a string receiver. Argument inputs
    /// can be either decimal-integer strings (turned into
    /// `Value::Number`) or quoted forms — the helper auto-detects
    /// to keep the existing test cases readable.
    fn call(method: &str, recv: &str, args: &[&str]) -> String {
        let mut interp = Interpreter::new();
        let recv_v = Value::string(JsString::from_str(recv, interp.gc_heap_mut()).unwrap());
        let arg_vs: Vec<Value> = args
            .iter()
            .map(|s| match s.parse::<i32>() {
                Ok(n) => Value::number(NumberValue::from_i32(n)),
                Err(_) => Value::string(JsString::from_str(s, interp.gc_heap_mut()).unwrap()),
            })
            .collect();
        let impl_fn = intrinsic_impl(method).unwrap();
        let result = {
            let mut ctx = NativeCtx::new_with_call_info(&mut interp, NativeCallInfo::call(recv_v));
            impl_fn(&mut ctx, &recv_v, &arg_vs).unwrap()
        };
        result.display_string(interp.gc_heap())
    }

    fn invoke_raw(
        method: &str,
        receiver: &Value,
        args: &[Value],
        interp: &mut Interpreter,
    ) -> Result<Value, NativeError> {
        let impl_fn = intrinsic_impl(method).unwrap();
        let mut ctx = NativeCtx::new_with_call_info(interp, NativeCallInfo::call(*receiver));
        impl_fn(&mut ctx, receiver, args)
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
        let mut interp = Interpreter::new();
        let err = invoke_raw("length", &Value::undefined(), &[], &mut interp).unwrap_err();
        assert!(matches!(err, NativeError::TypeError { .. }));
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
        let mut interp = Interpreter::new();
        call_v_with_interp(method, recv, args, &mut interp)
    }

    fn call_v_with_interp(method: &str, recv: &str, args: &[A], interp: &mut Interpreter) -> Value {
        let recv_v = Value::string(JsString::from_str(recv, interp.gc_heap_mut()).unwrap());
        let arg_vs: Vec<Value> = args
            .iter()
            .map(|a| match a {
                A::N(n) => Value::number(NumberValue::from_i32(*n)),
                A::S(s) => Value::string(JsString::from_str(s, interp.gc_heap_mut()).unwrap()),
            })
            .collect();
        invoke_raw(method, &recv_v, &arg_vs, interp).unwrap()
    }

    fn call_s(method: &str, recv: &str, args: &[A]) -> String {
        let mut interp = Interpreter::new();
        call_v_with_interp(method, recv, args, &mut interp).display_string(interp.gc_heap())
    }

    #[test]
    fn includes_returns_boolean() {
        assert_eq!(
            call_v("includes", "abcabc", &[A::S("bc")]),
            Value::boolean(true)
        );
        assert_eq!(
            call_v("includes", "abcabc", &[A::S("zz")]),
            Value::boolean(false)
        );
        // includes uses `from` argument like indexOf.
        assert_eq!(
            call_v("includes", "abcabc", &[A::S("bc"), A::N(2)]),
            Value::boolean(true)
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
        let mut interp = Interpreter::new();
        let recv = Value::string(JsString::from_str("a", interp.gc_heap_mut()).unwrap());
        let result = invoke_raw(
            "concat",
            &recv,
            &[Value::number(NumberValue::from_i32(3))],
            &mut interp,
        )
        .unwrap();
        let Some(s) = result.as_string(interp.gc_heap()) else {
            panic!("expected string result, got {result:?}");
        };
        assert_eq!(s.to_lossy_string(interp.gc_heap()), "a3");
    }

    #[test]
    fn repeat_basic() {
        assert_eq!(call_s("repeat", "abc", &[A::N(3)]), "abcabcabc");
        assert_eq!(call_s("repeat", "abc", &[A::N(0)]), "");
        assert_eq!(call_s("repeat", "", &[A::N(5)]), "");
    }

    #[test]
    fn repeat_rejects_negative() {
        let mut interp = Interpreter::new();
        let recv = Value::string(JsString::from_str("abc", interp.gc_heap_mut()).unwrap());
        let err = invoke_raw(
            "repeat",
            &recv,
            &[Value::number(NumberValue::from_i32(-1))],
            &mut interp,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            NativeError::TypeError { .. } | NativeError::RangeError { .. }
        ));
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
        assert_eq!(call_v("at", "abc", &[A::N(3)]), Value::undefined());
        assert_eq!(call_v("at", "abc", &[A::N(-4)]), Value::undefined());
    }

    #[test]
    fn code_point_at_basic() {
        // ASCII.
        assert_eq!(call_s("codePointAt", "abc", &[A::N(0)]), "97");
        // Out of range.
        assert_eq!(call_v("codePointAt", "abc", &[A::N(5)]), Value::undefined());
    }

    #[test]
    fn code_point_at_combines_surrogates() {
        // U+10000 = '𐀀' = 0xD800 0xDC00
        let mut interp = Interpreter::new();
        let units: [u16; 3] = [0xD800, 0xDC00, b'a' as u16];
        let recv = Value::string(JsString::from_utf16_units(&units, interp.gc_heap_mut()).unwrap());
        let r = invoke_raw(
            "codePointAt",
            &recv,
            &[Value::number(NumberValue::from_i32(0))],
            &mut interp,
        )
        .unwrap();
        assert_eq!(r.display_string(interp.gc_heap()), "65536");
        // Index 1 is the trailing surrogate alone.
        let r2 = invoke_raw(
            "codePointAt",
            &recv,
            &[Value::number(NumberValue::from_i32(1))],
            &mut interp,
        )
        .unwrap();
        assert_eq!(r2.display_string(interp.gc_heap()), "56320");
    }

    #[test]
    fn case_methods_unicode() {
        assert_eq!(call_s("toLowerCase", "ABC", &[]), "abc");
        assert_eq!(call_s("toUpperCase", "abc", &[]), "ABC");
        assert_eq!(call_s("toLowerCase", "Hello, World!", &[]), "hello, world!");
        // Non-ASCII folds per the Unicode default case mapping.
        let units: [u16; 3] = [0x00C9, b'a' as u16, b'b' as u16]; // 'É' + "ab"
        let mut interp = Interpreter::new();
        let recv = Value::string(JsString::from_utf16_units(&units, interp.gc_heap_mut()).unwrap());
        let r = invoke_raw("toLowerCase", &recv, &[], &mut interp).unwrap();
        let Some(s) = r.as_string(interp.gc_heap()) else {
            panic!("expected string");
        };
        let v = s.to_utf16_vec(interp.gc_heap());
        // 'É' (U+00C9) → 'é' (U+00E9).
        assert_eq!(v, vec![0x00E9, b'a' as u16, b'b' as u16]);
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
        let mut interp = Interpreter::new();
        let v = call_v_with_interp("split", "a,b,c", &[A::S(",")], &mut interp);
        let Some(a) = v.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(a, interp.gc_heap()), 3);
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 0).display_string(interp.gc_heap()),
            "a"
        );
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 1).display_string(interp.gc_heap()),
            "b"
        );
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 2).display_string(interp.gc_heap()),
            "c"
        );
    }

    #[test]
    fn split_consecutive_separators_yield_empty_chunks() {
        let mut interp = Interpreter::new();
        let v = call_v_with_interp("split", "a,,b", &[A::S(",")], &mut interp);
        let Some(a) = v.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(a, interp.gc_heap()), 3);
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 0).display_string(interp.gc_heap()),
            "a"
        );
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 1).display_string(interp.gc_heap()),
            ""
        );
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 2).display_string(interp.gc_heap()),
            "b"
        );
    }

    #[test]
    fn split_empty_separator_yields_code_units() {
        let mut interp = Interpreter::new();
        let v = call_v_with_interp("split", "abc", &[A::S("")], &mut interp);
        let Some(a) = v.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(a, interp.gc_heap()), 3);
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 0).display_string(interp.gc_heap()),
            "a"
        );
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 1).display_string(interp.gc_heap()),
            "b"
        );
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 2).display_string(interp.gc_heap()),
            "c"
        );
    }

    #[test]
    fn split_with_limit() {
        let mut interp = Interpreter::new();
        let v = call_v_with_interp("split", "a,b,c,d", &[A::S(","), A::N(2)], &mut interp);
        let Some(a) = v.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(a, interp.gc_heap()), 2);
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 0).display_string(interp.gc_heap()),
            "a"
        );
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 1).display_string(interp.gc_heap()),
            "b"
        );
    }

    #[test]
    fn split_no_match_returns_singleton() {
        let mut interp = Interpreter::new();
        let v = call_v_with_interp("split", "abc", &[A::S(",")], &mut interp);
        let Some(a) = v.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(a, interp.gc_heap()), 1);
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 0).display_string(interp.gc_heap()),
            "abc"
        );
    }

    #[test]
    fn split_empty_receiver() {
        // "".split(",") === [""]
        let mut interp = Interpreter::new();
        let v = call_v_with_interp("split", "", &[A::S(",")], &mut interp);
        let Some(a) = v.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(a, interp.gc_heap()), 1);
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 0).display_string(interp.gc_heap()),
            ""
        );

        // "".split("") === []
        let v2 = call_v_with_interp("split", "", &[A::S("")], &mut interp);
        {
            let Some(a) = v2.as_array() else {
                panic!("expected array");
            };
            assert_eq!(crate::array::len(a, interp.gc_heap()), 0);
        }
    }

    #[test]
    fn split_undefined_separator_returns_singleton() {
        // "abc".split() === ["abc"]
        let mut interp = Interpreter::new();
        let recv = Value::string(JsString::from_str("abc", interp.gc_heap_mut()).unwrap());
        let r = invoke_raw("split", &recv, &[], &mut interp).unwrap();
        let Some(a) = r.as_array() else {
            panic!("expected array");
        };
        assert_eq!(crate::array::len(a, interp.gc_heap()), 1);
        assert_eq!(
            crate::array::get(a, interp.gc_heap(), 0).display_string(interp.gc_heap()),
            "abc"
        );
    }
}
