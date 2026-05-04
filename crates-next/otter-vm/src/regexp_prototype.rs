//! `RegExp.prototype.*` intrinsic implementations.
//!
//! Slice 31. Method dispatch goes through the
//! [`crate::intrinsics`] table; property reads (`.source`,
//! `.flags`, `.global`, `.lastIndex`, …) handled at the
//! `Op::LoadProperty` site since they don't go through
//! `CallMethodValue`.
//!
//! # Contents
//! - [`REGEXP_PROTOTYPE_TABLE`] — declarative registry built with
//!   the `intrinsics!` macro.
//! - One private `impl_*` function per method.
//! - [`load_property`] — getter dispatch for non-method members.
//!
//! # Invariants
//! - Receivers are validated as `Value::RegExp`; non-regex
//!   receivers raise [`crate::intrinsics::IntrinsicError::BadReceiver`].
//! - `exec` and `test` honour the `g` and `y` flag semantics — both
//!   read and update `lastIndex`.
//! - `lastIndex` is clamped to `[0, len]` before any match attempt
//!   so a manual `re.lastIndex = -1` doesn't underflow.
//!
//! # See also
//! - [`docs/new-engine/tasks/31-regexp-and-pattern-methods.md`](
//!     ../../../docs/new-engine/tasks/31-regexp-and-pattern-methods.md
//!   )

use crate::Value;
use crate::array::JsArray;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::number::NumberValue;
use crate::regexp::JsRegExp;
use crate::string::JsString;

fn receiver_regexp<'a>(args: &'a IntrinsicArgs<'_>) -> Result<&'a JsRegExp, IntrinsicError> {
    match args.receiver {
        Value::RegExp(r) => Ok(r),
        _ => Err(IntrinsicError::BadReceiver { expected: "regexp" }),
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

/// Run a single match attempt and return the resulting JS array
/// (`[fullMatch, ...captureGroups]`) or `Value::Null` for no match.
/// Honours the `g` / `y` flag state stored on the receiver.
///
/// Per §22.2.7.2 [`RegExpBuiltinExec`](https://tc39.es/ecma262/#sec-regexpbuiltinexec)
/// the result array also carries `index`, `input`, and `groups`
/// own properties — and, when the receiver has the `d` flag, an
/// `indices` companion array (§22.2.7.7
/// [`MakeMatchIndicesIndexPairArray`](https://tc39.es/ecma262/#sec-makematchindicesindexpairarray)).
fn exec_once(
    re: &JsRegExp,
    text: &JsString,
    string_heap: &crate::string::StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, IntrinsicError> {
    let units = text.to_utf16_vec();
    let len = units.len();
    let flags = re.flags();
    let mut start = re.last_index() as usize;
    if (flags.global || flags.sticky) && start > len {
        re.set_last_index(0);
        return Ok(Value::Null);
    }
    if !flags.global && !flags.sticky {
        start = 0;
    }
    let m = re.regex().find_from_utf16(&units, start).next();
    let m = match m {
        Some(m) => m,
        None => {
            if flags.global || flags.sticky {
                re.set_last_index(0);
            }
            return Ok(Value::Null);
        }
    };
    if flags.sticky && m.range.start != start {
        re.set_last_index(0);
        return Ok(Value::Null);
    }
    if flags.global || flags.sticky {
        re.set_last_index(m.range.end as u32);
    }

    Ok(Value::Array(build_match_result(
        &m,
        &units,
        text,
        flags.has_indices,
        string_heap,
        gc_heap,
    )?))
}

/// §22.2.7.2 steps 26–32 — build the JS-visible match-result array
/// out of a `regress::Match`. Used by `RegExp.prototype.exec` and
/// reused by `String.prototype.match` / `.matchAll` so both surfaces
/// produce identical shapes (full match + capture slots, plus
/// `index` / `input` / `groups` / optionally `indices`).
pub(crate) fn build_match_result(
    m: &regress::Match,
    units: &[u16],
    input: &JsString,
    has_indices: bool,
    string_heap: &crate::string::StringHeap,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<JsArray, IntrinsicError> {
    let full = JsString::from_utf16_units(&units[m.range.clone()], string_heap)?;
    let mut out: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
    out.push(Value::String(full));
    for cap in &m.captures {
        match cap {
            Some(r) => {
                let s = JsString::from_utf16_units(&units[r.clone()], string_heap)?;
                out.push(Value::String(s));
            }
            None => out.push(Value::Undefined),
        }
    }
    let arr = JsArray::from_elements(out);

    arr.set_named_property(
        "index",
        Value::Number(NumberValue::from_i32(m.range.start as i32)),
    );
    arr.set_named_property("input", Value::String(input.clone()));

    let mut named_iter = m.named_groups();
    let first_named = named_iter.next();
    if let Some((name, range)) = first_named {
        let groups_obj = crate::object::alloc_object(gc_heap)?;
        crate::object::set_prototype(groups_obj, gc_heap, None);
        let value = match range {
            Some(r) => Value::String(JsString::from_utf16_units(&units[r], string_heap)?),
            None => Value::Undefined,
        };
        crate::object::set(groups_obj, gc_heap, name, value);
        for (name, range) in named_iter {
            let value = match range {
                Some(r) => Value::String(JsString::from_utf16_units(&units[r], string_heap)?),
                None => Value::Undefined,
            };
            crate::object::set(groups_obj, gc_heap, name, value);
        }
        arr.set_named_property("groups", Value::Object(groups_obj));
    } else {
        arr.set_named_property("groups", Value::Undefined);
    }

    if has_indices {
        let mut indices_elems: Vec<Value> = Vec::with_capacity(1 + m.captures.len());
        indices_elems.push(pair_array(m.range.start, m.range.end));
        for cap in &m.captures {
            match cap {
                Some(r) => indices_elems.push(pair_array(r.start, r.end)),
                None => indices_elems.push(Value::Undefined),
            }
        }
        let indices_arr = JsArray::from_elements(indices_elems);
        let mut named_iter = m.named_groups();
        let first_named = named_iter.next();
        if let Some((name, range)) = first_named {
            let g_obj = crate::object::alloc_object(gc_heap)?;
            crate::object::set_prototype(g_obj, gc_heap, None);
            let v = match range {
                Some(r) => pair_array(r.start, r.end),
                None => Value::Undefined,
            };
            crate::object::set(g_obj, gc_heap, name, v);
            for (name, range) in named_iter {
                let v = match range {
                    Some(r) => pair_array(r.start, r.end),
                    None => Value::Undefined,
                };
                crate::object::set(g_obj, gc_heap, name, v);
            }
            indices_arr.set_named_property("groups", Value::Object(g_obj));
        } else {
            indices_arr.set_named_property("groups", Value::Undefined);
        }
        arr.set_named_property("indices", Value::Array(indices_arr));
    }
    Ok(arr)
}

/// Build a `[start, end]` two-element array used by the `d`-flag
/// indices companion (§22.2.7.7).
fn pair_array(start: usize, end: usize) -> Value {
    Value::Array(JsArray::from_elements([
        Value::Number(NumberValue::from_i32(start as i32)),
        Value::Number(NumberValue::from_i32(end as i32)),
    ]))
}

fn impl_exec(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let re = receiver_regexp(args)?;
    let text = arg_string(args, 0)?.clone();
    let re_clone = re.clone();
    let mut heap = args.gc_heap.borrow_mut();
    exec_once(&re_clone, &text, args.string_heap, *heap)
}

fn impl_test(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let re = receiver_regexp(args)?;
    let text = arg_string(args, 0)?.clone();
    let re_clone = re.clone();
    let mut heap = args.gc_heap.borrow_mut();
    let result = exec_once(&re_clone, &text, args.string_heap, *heap)?;
    Ok(Value::Boolean(!matches!(result, Value::Null)))
}

fn impl_to_string(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let re = receiver_regexp(args)?;
    let rendered = format!("/{}/{}", re.source(), re.flags().to_js_string());
    Ok(Value::String(JsString::from_str(
        &rendered,
        args.string_heap,
    )?))
}

/// Declarative `RegExp.prototype` table.
pub static REGEXP_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            RegExp,
            "exec"     / 1 => impl_exec,
            "test"     / 1 => impl_test,
            "toString" / 0 => impl_to_string,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    REGEXP_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::RegExp, name)
}

/// Resolve a JS-visible property of a `RegExp` value. `None` when
/// the property is not a recognised RegExp member — callers fall
/// back to `Value::Undefined`. `lastIndex` reads and writes flow
/// through here too.
#[must_use]
pub fn load_property(re: &JsRegExp, name: &str, string_heap: &crate::string::StringHeap) -> Value {
    match name {
        "source" => match JsString::from_str(re.source(), string_heap) {
            Ok(s) => Value::String(s),
            Err(_) => Value::Undefined,
        },
        "flags" => match JsString::from_str(&re.flags().to_js_string(), string_heap) {
            Ok(s) => Value::String(s),
            Err(_) => Value::Undefined,
        },
        "hasIndices" => Value::Boolean(re.flags().has_indices),
        "global" => Value::Boolean(re.flags().global),
        "ignoreCase" => Value::Boolean(re.flags().ignore_case),
        "multiline" => Value::Boolean(re.flags().multiline),
        "dotAll" => Value::Boolean(re.flags().dot_all),
        "unicode" => Value::Boolean(re.flags().unicode),
        "sticky" => Value::Boolean(re.flags().sticky),
        "unicodeSets" => Value::Boolean(re.flags().unicode_sets),
        "lastIndex" => Value::Number(NumberValue::from_i32(re.last_index() as i32)),
        _ => Value::Undefined,
    }
}

/// Mutate a JS-visible property on a `RegExp`. Currently only
/// `lastIndex` is writable; everything else is silently ignored
/// (foundation: the spec marks accessors non-writable, so a real
/// `TypeError` belongs in a later strict-mode slice).
pub fn store_property(re: &JsRegExp, name: &str, value: &Value) {
    if name == "lastIndex" {
        if let Value::Number(n) = value {
            let raw = n.as_f64();
            let clamped = if raw.is_nan() || raw < 0.0 {
                0
            } else if raw > u32::MAX as f64 {
                u32::MAX
            } else {
                raw as u32
            };
            re.set_last_index(clamped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::StringHeap;

    fn make(pattern: &str, flags: &str) -> Value {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        Value::RegExp(JsRegExp::compile(&units, flags).unwrap())
    }

    fn call(method: &str, recv: &Value, args: &[Value]) -> Value {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let entry = lookup(method).unwrap();
        (entry.impl_fn)(&IntrinsicArgs {
            receiver: recv,
            args,
            string_heap: &heap,
            gc_heap: std::cell::RefCell::new(&mut gc_heap),
        })
        .unwrap()
    }

    #[test]
    fn test_returns_boolean() {
        let heap = StringHeap::default();
        let re = make("ab+c", "");
        let text = Value::String(JsString::from_str("abbbc", &heap).unwrap());
        assert_eq!(call("test", &re, &[text]), Value::Boolean(true));
        let no = Value::String(JsString::from_str("xy", &heap).unwrap());
        assert_eq!(call("test", &re, &[no]), Value::Boolean(false));
    }

    #[test]
    fn exec_returns_array_or_null() {
        let heap = StringHeap::default();
        let re = make("(a)(b)", "");
        let text = Value::String(JsString::from_str("ab", &heap).unwrap());
        let r = call("exec", &re, &[text]);
        match r {
            Value::Array(arr) => {
                assert_eq!(arr.len(), 3);
                assert_eq!(arr.get(0).display_string(), "ab");
                assert_eq!(arr.get(1).display_string(), "a");
                assert_eq!(arr.get(2).display_string(), "b");
            }
            _ => panic!("expected array"),
        }
        let miss = call(
            "exec",
            &re,
            &[Value::String(JsString::from_str("zz", &heap).unwrap())],
        );
        assert_eq!(miss, Value::Null);
    }

    #[test]
    fn exec_global_walks_through_text() {
        let heap = StringHeap::default();
        let re = make("a", "g");
        let text = Value::String(JsString::from_str("abab", &heap).unwrap());
        // First call → match at 0, lastIndex moves to 1.
        let r1 = call("exec", &re, std::slice::from_ref(&text));
        match (&r1, &re) {
            (Value::Array(arr), Value::RegExp(rx)) => {
                assert_eq!(arr.get(0).display_string(), "a");
                assert_eq!(rx.last_index(), 1);
            }
            _ => panic!(),
        }
        // Second call → match at 2, lastIndex → 3.
        call("exec", &re, std::slice::from_ref(&text));
        if let Value::RegExp(rx) = &re {
            assert_eq!(rx.last_index(), 3);
        }
        // Third call → no match, lastIndex → 0.
        let r3 = call("exec", &re, &[text]);
        assert_eq!(r3, Value::Null);
        if let Value::RegExp(rx) = &re {
            assert_eq!(rx.last_index(), 0);
        }
    }

    #[test]
    fn property_lookups() {
        let heap = StringHeap::default();
        let re = JsRegExp::compile(&"ab+c".encode_utf16().collect::<Vec<_>>(), "gi").unwrap();
        let src = load_property(&re, "source", &heap);
        assert_eq!(src.display_string(), "ab+c");
        let flags = load_property(&re, "flags", &heap);
        assert_eq!(flags.display_string(), "gi");
        assert_eq!(load_property(&re, "global", &heap), Value::Boolean(true));
        assert_eq!(
            load_property(&re, "ignoreCase", &heap),
            Value::Boolean(true)
        );
        assert_eq!(
            load_property(&re, "multiline", &heap),
            Value::Boolean(false)
        );
    }

    #[test]
    fn last_index_writable() {
        let heap = StringHeap::default();
        let re = JsRegExp::compile(&"a".encode_utf16().collect::<Vec<_>>(), "g").unwrap();
        store_property(&re, "lastIndex", &Value::Number(NumberValue::from_i32(7)));
        assert_eq!(re.last_index(), 7);
        // Negative clamps to 0.
        store_property(&re, "lastIndex", &Value::Number(NumberValue::from_i32(-3)));
        assert_eq!(re.last_index(), 0);
        // String writes are ignored (non-spec, but defensive).
        store_property(
            &re,
            "lastIndex",
            &Value::String(JsString::from_str("x", &heap).unwrap()),
        );
        assert_eq!(re.last_index(), 0);
        // Non-lastIndex names are silently ignored.
        store_property(
            &re,
            "source",
            &Value::String(JsString::from_str("nope", &heap).unwrap()),
        );
    }
}
