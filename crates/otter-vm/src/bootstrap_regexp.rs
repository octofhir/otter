//! ECMA-262 §22.2 RegExp bootstrap installer.
//!
//! Installs the JS-visible `RegExp` constructor + prototype.
//! The prototype carries `exec` / `test` / `toString` / `compile`
//! as own data properties and the spec-mandated accessor getters
//! for `source` / `flags` / `global` / `ignoreCase` / `multiline` /
//! `dotAll` / `unicode` / `sticky` / `hasIndices` / `unicodeSets`.
//! `@@toStringTag = "RegExp"` is installed by
//! [`install_regexp_well_knowns_post_bootstrap`].
//!
//! # Contents
//! - [`install_regexp`] — bootstrap entry.
//! - [`install_regexp_well_knowns_post_bootstrap`] — symbol fixup.
//!
//! # Invariants
//! - `new RegExp(pattern, flags)` and bare `RegExp(...)` both
//!   produce a fresh `Value::RegExp`. Per §22.2.3.1 step 1, when
//!   `pattern` is itself a RegExp and `flags` is `undefined` and
//!   the new-target is the active `RegExp` constructor, the
//!   incoming RegExp is returned unchanged.
//! - The prototype accessor getters throw `TypeError` when `this`
//!   is not a `Value::RegExp` (with the spec-mandated exception
//!   that `RegExp.prototype` itself returns the sentinel values
//!   `""` for `source` and `""` for `flags`).
//! - The prototype intrinsic fast-path at the `Op::CallMethod`
//!   dispatcher still handles `re.exec(...)` / `re.test(...)` for
//!   speed; the installed `NativeFunction` properties are reached
//!   only by reflective access (`Object.getOwnPropertyDescriptor`,
//!   `Function.prototype.call`, etc.).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-regexp-constructor>
//! - <https://tc39.es/ecma262/#sec-properties-of-the-regexp-prototype-object>

use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::regexp::{JsRegExp, RegExpFlags};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

/// `BuiltinIntrinsic` adapter for the global `RegExp` constructor.
pub struct Intrinsic;

impl crate::intrinsic_install::BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "RegExp";
    const FEATURE: crate::bootstrap::BootstrapFeatures = crate::bootstrap::BootstrapFeatures::CORE;

    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install(heap, global)
    }
}

/// §22.2 RegExp — installer body, called through [`Intrinsic`].
fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    let global_root = Value::Object(global);
    let prototype = crate::bootstrap::alloc_object_with_value_roots(heap, &[&global_root])?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }

    install_prototype_methods(heap, prototype, vec![global_root.clone()])?;
    install_prototype_accessors(heap, prototype, vec![global_root.clone()])?;

    let prototype_root = Value::Object(prototype);
    let ctor = crate::bootstrap::native_constructor_static_with_value_roots(
        heap,
        "RegExp",
        2,
        regexp_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let string_heap = crate::string::StringHeap::default();
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }

    // §22.2.5 — `RegExp.prototype.constructor` data property.
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::NativeFunction(ctor), true, false, true),
    );

    crate::bootstrap::define_global_value(
        global,
        heap,
        <Intrinsic as crate::intrinsic_install::BuiltinIntrinsic>::NAME,
        Value::NativeFunction(ctor),
    );
    Ok(())
}

/// Install `@@toStringTag` (no longer set on every RegExp — it
/// lives on the prototype) and the future Symbol-keyed methods
/// (`@@match`, `@@replace`, `@@search`, `@@split`, `@@matchAll`)
/// once the per-realm well-known table is materialised. The
/// Symbol-keyed methods stay foundation-driven through
/// `String.prototype.{match,replace,search,split,matchAll}` for
/// now — only `@@toStringTag` is landed here.
pub fn install_regexp_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    let Some(Value::NativeFunction(ctor)) = object::get(global, heap, "RegExp") else {
        return Ok(());
    };
    let descriptor = ctor
        .own_property_descriptor(heap, string_heap, "prototype")
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let prototype = match descriptor.and_then(|d| match d.kind {
        crate::object::DescriptorKind::Data {
            value: Value::Object(p),
        } => Some(p),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };
    // §22.2.6.{8,10,11} `RegExp.prototype[@@match]` /
    // `RegExp.prototype[@@search]` / `RegExp.prototype[@@replace]` —
    // install native functions so user calls like
    // `re[Symbol.match]("…")` / `re[Symbol.search]("…")` /
    // `re[Symbol.replace]("…", repl)` resolve through the
    // spec-mandated algorithm. `@@matchAll` and `@@split` remain
    // foundation-driven through their `String.prototype.*`
    // counterparts and will land in follow-up commits.
    let prototype_root = Value::Object(prototype);
    let match_sym = well_known.get(WellKnown::Match);
    let search_sym = well_known.get(WellKnown::Search);
    let replace_sym = well_known.get(WellKnown::Replace);
    let split_sym = well_known.get(WellKnown::Split);
    let match_all_sym = well_known.get(WellKnown::MatchAll);
    let match_fn = crate::bootstrap::native_static_with_value_roots(
        heap,
        "[Symbol.match]",
        1,
        crate::regexp_prototype::native_regexp_symbol_match,
        &[&prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let search_fn = crate::bootstrap::native_static_with_value_roots(
        heap,
        "[Symbol.search]",
        1,
        crate::regexp_prototype::native_regexp_symbol_search,
        &[&prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let replace_fn = crate::bootstrap::native_static_with_value_roots(
        heap,
        "[Symbol.replace]",
        2,
        crate::regexp_prototype::native_regexp_symbol_replace,
        &[&prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let split_fn = crate::bootstrap::native_static_with_value_roots(
        heap,
        "[Symbol.split]",
        2,
        crate::regexp_prototype::native_regexp_symbol_split,
        &[&prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let match_all_fn = crate::bootstrap::native_static_with_value_roots(
        heap,
        "[Symbol.matchAll]",
        1,
        crate::regexp_prototype::native_regexp_symbol_match_all,
        &[&prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &match_sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::NativeFunction(match_fn)),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &search_sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::NativeFunction(search_fn)),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &replace_sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::NativeFunction(replace_fn)),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &split_sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::NativeFunction(split_fn)),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &match_all_sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::NativeFunction(match_all_fn)),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    let _ = string_heap;
    Ok(())
}

// ---------------------------------------------------------------
// Constructor body
// ---------------------------------------------------------------

/// §22.2.3.1 RegExp(pattern, flags).
fn regexp_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pattern_arg = args.first().cloned().unwrap_or(Value::Undefined);
    let flags_arg = args.get(1).cloned().unwrap_or(Value::Undefined);

    // Step 1 — if `pattern` is a RegExp and (called without new
    // OR flags is undefined), reuse the existing handle when the
    // active constructor matches. Foundation: always honour the
    // identity for bare `RegExp(re)` with undefined flags.
    if let Value::RegExp(existing) = &pattern_arg
        && matches!(flags_arg, Value::Undefined)
        && !ctx.is_construct_call()
    {
        return Ok(Value::RegExp(*existing));
    }

    let heap = ctx.heap_mut();
    // Source + flag string preparation.
    let (pattern_utf16, flags_str): (Vec<u16>, String) = match (&pattern_arg, &flags_arg) {
        // RegExp + RegExp source clone.
        (Value::RegExp(re), Value::Undefined) => {
            (re.pattern_utf16(heap), re.flags(heap).to_js_string())
        }
        (Value::RegExp(re), Value::String(s)) => (re.pattern_utf16(heap), s.to_lossy_string()),
        // String + flag.
        (Value::String(s), flags) => {
            let units = s.to_utf16_vec();
            let f = match flags {
                Value::Undefined => String::new(),
                Value::String(fs) => fs.to_lossy_string(),
                other => other.display_string(),
            };
            (units, f)
        }
        // Other source → ToString.
        (Value::Undefined, flags) => {
            let f = match flags {
                Value::Undefined => String::new(),
                Value::String(fs) => fs.to_lossy_string(),
                other => other.display_string(),
            };
            (Vec::new(), f)
        }
        (other, flags) => {
            let pattern_str = other.display_string();
            let f = match flags {
                Value::Undefined => String::new(),
                Value::String(fs) => fs.to_lossy_string(),
                other => other.display_string(),
            };
            (pattern_str.encode_utf16().collect(), f)
        }
    };

    let re = JsRegExp::compile(heap, &pattern_utf16, &flags_str).map_err(|err| {
        NativeError::SyntaxError {
            name: "RegExp",
            reason: format!("{err}"),
        }
    })?;
    Ok(Value::RegExp(re))
}

// ---------------------------------------------------------------
// Prototype method bodies (delegate to existing intrinsic impls)
// ---------------------------------------------------------------

fn install_prototype_methods(
    heap: &mut otter_gc::GcHeap,
    prototype: JsObject,
    value_roots: Vec<Value>,
) -> Result<(), JsSurfaceError> {
    let mut builder = ObjectBuilder::from_object_with_value_roots(heap, prototype, value_roots);
    builder.method(
        "exec",
        1,
        NativeCall::Static(proto_exec),
        Attr::builtin_function(),
    )?;
    builder.method(
        "test",
        1,
        NativeCall::Static(proto_test),
        Attr::builtin_function(),
    )?;
    builder.method(
        "toString",
        0,
        NativeCall::Static(proto_to_string),
        Attr::builtin_function(),
    )?;
    builder.method(
        "compile",
        2,
        NativeCall::Static(proto_compile),
        Attr::builtin_function(),
    )?;
    Ok(())
}

fn proto_exec(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let re = receiver_regexp(ctx, "RegExp.prototype.exec")?;
    let text = args.first().cloned().unwrap_or(Value::Undefined);
    let text_str = coerce_to_string(ctx, &text, "RegExp.prototype.exec")?;
    let string_heap = ctx.interp_mut().string_heap_clone();
    crate::regexp_prototype::exec_once_native(&re, &text_str, &string_heap, ctx, &[args])
        .map_err(|e| intrinsic_to_native(e, "RegExp.prototype.exec"))
}

fn proto_test(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let result = proto_exec(ctx, args)?;
    Ok(Value::Boolean(!matches!(result, Value::Null)))
}

/// §B.2.4.1 `RegExp.prototype.compile(pattern, flags)` — native
/// surface that mirrors the intrinsic-table dispatch path for users
/// who call through `Function.prototype.call` / property reads.
fn proto_compile(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let re = receiver_regexp(ctx, "RegExp.prototype.compile")?;
    let pattern_raw = args.first().cloned().unwrap_or(Value::Undefined);
    let flags_raw = args.get(1).cloned().unwrap_or(Value::Undefined);
    let (pattern_units, flags_str) = match pattern_raw {
        Value::RegExp(other) => {
            if !matches!(flags_raw, Value::Undefined) {
                return Err(NativeError::TypeError {
                    name: "RegExp.prototype.compile",
                    reason: "Cannot supply flags when constructing one RegExp from another"
                        .to_string(),
                });
            }
            let heap = ctx.heap();
            (other.pattern_utf16(heap), other.flags(heap).to_js_string())
        }
        Value::Undefined => (Vec::<u16>::new(), value_to_text(&flags_raw, "RegExp.prototype.compile")?),
        ref other => {
            let pattern_str = value_to_text(other, "RegExp.prototype.compile")?;
            let pattern_units: Vec<u16> = pattern_str.encode_utf16().collect();
            let flags_text = value_to_text(&flags_raw, "RegExp.prototype.compile")?;
            (pattern_units, flags_text)
        }
    };
    re.reinitialize(ctx.heap_mut(), &pattern_units, &flags_str)
        .map_err(|err| NativeError::TypeError {
            name: "RegExp.prototype.compile",
            reason: match err {
                crate::regexp::RegExpError::InvalidPattern { message } => {
                    format!("invalid regular expression: {message}")
                }
                _ => "invalid regular expression flag".to_string(),
            },
        })?;
    Ok(Value::RegExp(re))
}

fn value_to_text(value: &Value, name: &'static str) -> Result<String, NativeError> {
    Ok(match value {
        Value::String(s) => s.to_lossy_string(),
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Boolean(true) => "true".to_string(),
        Value::Boolean(false) => "false".to_string(),
        Value::Number(n) => n.to_display_string(),
        Value::BigInt(b) => b.to_decimal_string(),
        Value::Symbol(_) => {
            return Err(NativeError::TypeError {
                name,
                reason: "cannot convert a Symbol to a string".to_string(),
            });
        }
        other => other.display_string(),
    })
}

fn proto_to_string(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let re = receiver_regexp(ctx, "RegExp.prototype.toString")?;
    let source = re.source(ctx.heap());
    let flags = re.flags(ctx.heap()).to_js_string();
    let rendered = format!("/{source}/{flags}");
    let string_heap = ctx.interp_mut().string_heap_clone();
    let s = JsString::from_str(&rendered, &string_heap)
        .map_err(|_| oom("RegExp.prototype.toString"))?;
    Ok(Value::String(s))
}

// ---------------------------------------------------------------
// Prototype accessors
// ---------------------------------------------------------------

fn install_prototype_accessors(
    heap: &mut otter_gc::GcHeap,
    prototype: JsObject,
    value_roots: Vec<Value>,
) -> Result<(), JsSurfaceError> {
    install_accessor(heap, prototype, "source", accessor_source, &value_roots)?;
    install_accessor(heap, prototype, "flags", accessor_flags, &value_roots)?;
    install_accessor(heap, prototype, "global", accessor_global, &value_roots)?;
    install_accessor(
        heap,
        prototype,
        "ignoreCase",
        accessor_ignore_case,
        &value_roots,
    )?;
    install_accessor(
        heap,
        prototype,
        "multiline",
        accessor_multiline,
        &value_roots,
    )?;
    install_accessor(heap, prototype, "dotAll", accessor_dot_all, &value_roots)?;
    install_accessor(heap, prototype, "unicode", accessor_unicode, &value_roots)?;
    install_accessor(heap, prototype, "sticky", accessor_sticky, &value_roots)?;
    install_accessor(
        heap,
        prototype,
        "hasIndices",
        accessor_has_indices,
        &value_roots,
    )?;
    install_accessor(
        heap,
        prototype,
        "unicodeSets",
        accessor_unicode_sets,
        &value_roots,
    )?;
    Ok(())
}

fn install_accessor(
    heap: &mut otter_gc::GcHeap,
    prototype: JsObject,
    name: &'static str,
    call: crate::native_function::NativeFastFn,
    value_roots: &[Value],
) -> Result<(), JsSurfaceError> {
    let prototype_root = Value::Object(prototype);
    let mut roots = Vec::with_capacity(value_roots.len() + 1);
    roots.push(&prototype_root);
    roots.extend(value_roots.iter());
    let getter =
        crate::bootstrap::native_static_with_value_roots(heap, name, 0, call, roots.as_slice())
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let desc = PropertyDescriptor::accessor(Some(Value::NativeFunction(getter)), None, false, true);
    if !object::define_own_property(prototype, heap, name, desc) {
        return Err(JsSurfaceError::DefinePropertyFailed(name));
    }
    Ok(())
}

/// §22.2.6.10 `get RegExp.prototype.source`. When `this` is the
/// realm's `%RegExp.prototype%` (no `[[OriginalSource]]` slot)
/// returns the sentinel `"(?:)"`; non-RegExp non-prototype
/// receivers throw `TypeError`. Otherwise emits the spec's
/// `EscapeRegExpPattern(src, flags)`: empty source → `"(?:)"`,
/// unescaped `/` / line terminators escaped.
fn accessor_source(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let receiver = ctx.this_value().clone();
    let string_heap = ctx.interp_mut().string_heap_clone();
    let raw = match &receiver {
        Value::RegExp(re) => re.source(ctx.heap()),
        Value::Object(obj) => {
            let is_proto = ctx
                .interp_mut()
                .constructor_prototype_value("RegExp")
                .ok()
                .and_then(|p| match p {
                    Value::Object(p) => Some(p),
                    _ => None,
                })
                .is_some_and(|p| p == *obj);
            if !is_proto {
                return Err(NativeError::TypeError {
                    name: "get RegExp.prototype.source",
                    reason: "this is not a RegExp".to_string(),
                });
            }
            return Ok(Value::String(
                JsString::from_str("(?:)", &string_heap).map_err(|_| oom("source"))?,
            ));
        }
        _ => {
            return Err(NativeError::TypeError {
                name: "get RegExp.prototype.source",
                reason: "this is not a RegExp".to_string(),
            });
        }
    };
    let escaped = crate::regexp_prototype::escape_regexp_pattern(&raw);
    Ok(Value::String(
        JsString::from_str(&escaped, &string_heap).map_err(|_| oom("source"))?,
    ))
}

/// §22.2.6.4 `get RegExp.prototype.flags`. Generic over any
/// receiver: reads each flag property via `[[Get]]`, applies
/// `ToBoolean`, and concatenates the flag letter when truthy.
/// Spec order is `d g i m s u v y` (hasIndices, global, ignoreCase,
/// multiline, dotAll, unicode, unicodeSets, sticky).
fn accessor_flags(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let receiver = ctx.this_value().clone();
    if !matches!(
        receiver,
        Value::Object(_)
            | Value::RegExp(_)
            | Value::Proxy(_)
            | Value::Array(_)
            | Value::Function { .. }
            | Value::Closure { .. }
            | Value::NativeFunction(_)
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::WeakMap(_)
            | Value::WeakSet(_)
            | Value::WeakRef(_)
            | Value::FinalizationRegistry(_)
            | Value::Promise(_)
            | Value::ArrayBuffer(_)
            | Value::DataView(_)
            | Value::TypedArray(_)
    ) {
        return Err(NativeError::TypeError {
            name: "get RegExp.prototype.flags",
            reason: "this value must be an Object".to_string(),
        });
    }
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| NativeError::TypeError {
        name: "get RegExp.prototype.flags",
        reason: "missing execution context".to_string(),
    })?;
    let mut out = String::with_capacity(8);
    let map_err = |e: crate::VmError| match e {
        crate::VmError::Uncaught { value } => NativeError::Thrown {
            name: "get RegExp.prototype.flags",
            message: value,
        },
        crate::VmError::TypeError { message } => NativeError::TypeError {
            name: "get RegExp.prototype.flags",
            reason: message,
        },
        other => NativeError::TypeError {
            name: "get RegExp.prototype.flags",
            reason: other.to_string(),
        },
    };
    for &(prop, letter) in &[
        ("hasIndices", 'd'),
        ("global", 'g'),
        ("ignoreCase", 'i'),
        ("multiline", 'm'),
        ("dotAll", 's'),
        ("unicode", 'u'),
        ("unicodeSets", 'v'),
        ("sticky", 'y'),
    ] {
        let outcome = interp
            .ordinary_get_value(
                &exec,
                receiver.clone(),
                receiver.clone(),
                &crate::VmPropertyKey::String(prop),
                0,
            )
            .map_err(map_err)?;
        let value = match outcome {
            crate::VmGetOutcome::Value(v) => v,
            crate::VmGetOutcome::InvokeGetter { getter } => {
                let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                interp
                    .run_callable_sync(&exec, &getter, receiver.clone(), args)
                    .map_err(map_err)?
            }
        };
        if value.to_boolean() {
            out.push(letter);
        }
    }
    let string_heap = ctx.interp_mut().string_heap_clone();
    Ok(Value::String(
        JsString::from_str(&out, &string_heap).map_err(|_| oom("flags"))?,
    ))
}

fn accessor_global(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    flag_bool(ctx, "get RegExp.prototype.global", |f| f.global)
}

fn accessor_ignore_case(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    flag_bool(ctx, "get RegExp.prototype.ignoreCase", |f| f.ignore_case)
}

fn accessor_multiline(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    flag_bool(ctx, "get RegExp.prototype.multiline", |f| f.multiline)
}

fn accessor_dot_all(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    flag_bool(ctx, "get RegExp.prototype.dotAll", |f| f.dot_all)
}

fn accessor_unicode(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    flag_bool(ctx, "get RegExp.prototype.unicode", |f| f.unicode)
}

fn accessor_sticky(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    flag_bool(ctx, "get RegExp.prototype.sticky", |f| f.sticky)
}

fn accessor_has_indices(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    flag_bool(ctx, "get RegExp.prototype.hasIndices", |f| f.has_indices)
}

fn accessor_unicode_sets(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    flag_bool(ctx, "get RegExp.prototype.unicodeSets", |f| f.unicode_sets)
}

fn flag_bool(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
    f: impl FnOnce(&RegExpFlags) -> bool,
) -> Result<Value, NativeError> {
    let re = receiver_regexp(ctx, name)?;
    let flags = re.flags(ctx.heap());
    Ok(Value::Boolean(f(&flags)))
}

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn receiver_regexp(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsRegExp, NativeError> {
    match ctx.this_value() {
        Value::RegExp(r) => Ok(*r),
        _ => Err(NativeError::TypeError {
            name,
            reason: "this is not a RegExp".to_string(),
        }),
    }
}

fn coerce_to_string(
    ctx: &mut NativeCtx<'_>,
    v: &Value,
    name: &'static str,
) -> Result<JsString, NativeError> {
    if let Value::String(s) = v {
        return Ok(s.clone());
    }
    let s = v.display_string();
    let string_heap = ctx.interp_mut().string_heap_clone();
    JsString::from_str(&s, &string_heap).map_err(|_| oom(name))
}

fn oom(name: &'static str) -> NativeError {
    NativeError::TypeError {
        name,
        reason: "out of memory".to_string(),
    }
}

fn intrinsic_to_native(err: crate::intrinsics::IntrinsicError, name: &'static str) -> NativeError {
    NativeError::TypeError {
        name,
        reason: err.to_string(),
    }
}
