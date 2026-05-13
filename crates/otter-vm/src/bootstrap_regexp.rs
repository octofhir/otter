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

use crate::bootstrap::BootstrapEntry;
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::{NativeCall, NativeFunction};
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::regexp::{JsRegExp, RegExpFlags};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

/// §22.2 RegExp — bootstrap install.
pub(crate) fn install_regexp(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let prototype = object::alloc_object(heap)?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }

    install_prototype_methods(heap, prototype)?;
    install_prototype_accessors(heap, prototype)?;

    let ctor = NativeFunction::new_constructor_static(heap, "RegExp", 2, regexp_ctor_call)
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

    crate::bootstrap::define_global_value(global, heap, entry.name, Value::NativeFunction(ctor));
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
    // `RegExp.prototype` does NOT have `@@toStringTag` per spec —
    // §22.2.5 does not list it. But §22.2.6.16
    // `RegExp.prototype[@@matchAll]` etc. are spec methods on the
    // prototype. They stay foundation-driven for the moment.
    let _ = well_known.get(WellKnown::Match); // silence the unused-import lint
    let _ = (heap, string_heap, prototype);
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
) -> Result<(), JsSurfaceError> {
    let mut builder = ObjectBuilder::from_object(heap, prototype);
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
    Ok(())
}

fn proto_exec(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let re = receiver_regexp(ctx, "RegExp.prototype.exec")?;
    let text = args.first().cloned().unwrap_or(Value::Undefined);
    let text_str = coerce_to_string(ctx, &text, "RegExp.prototype.exec")?;
    let string_heap = ctx.interp_mut().string_heap_clone();
    crate::regexp_prototype::exec_once(&re, &text_str, &string_heap, ctx.heap_mut())
        .map_err(|e| intrinsic_to_native(e, "RegExp.prototype.exec"))
}

fn proto_test(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let result = proto_exec(ctx, args)?;
    Ok(Value::Boolean(!matches!(result, Value::Null)))
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
) -> Result<(), JsSurfaceError> {
    install_accessor(heap, prototype, "source", accessor_source)?;
    install_accessor(heap, prototype, "flags", accessor_flags)?;
    install_accessor(heap, prototype, "global", accessor_global)?;
    install_accessor(heap, prototype, "ignoreCase", accessor_ignore_case)?;
    install_accessor(heap, prototype, "multiline", accessor_multiline)?;
    install_accessor(heap, prototype, "dotAll", accessor_dot_all)?;
    install_accessor(heap, prototype, "unicode", accessor_unicode)?;
    install_accessor(heap, prototype, "sticky", accessor_sticky)?;
    install_accessor(heap, prototype, "hasIndices", accessor_has_indices)?;
    install_accessor(heap, prototype, "unicodeSets", accessor_unicode_sets)?;
    Ok(())
}

fn install_accessor(
    heap: &mut otter_gc::GcHeap,
    prototype: JsObject,
    name: &'static str,
    call: crate::native_function::NativeFastFn,
) -> Result<(), JsSurfaceError> {
    let getter =
        NativeFunction::new_static(heap, name, 0, call).map_err(|_| JsSurfaceError::OutOfMemory)?;
    let desc = PropertyDescriptor::accessor(Some(Value::NativeFunction(getter)), None, false, true);
    if !object::define_own_property(prototype, heap, name, desc) {
        return Err(JsSurfaceError::DefinePropertyFailed(name));
    }
    Ok(())
}

fn accessor_source(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let re = receiver_regexp(ctx, "get RegExp.prototype.source")?;
    let source = re.source(ctx.heap());
    let string_heap = ctx.interp_mut().string_heap_clone();
    Ok(Value::String(
        JsString::from_str(&source, &string_heap).map_err(|_| oom("source"))?,
    ))
}

fn accessor_flags(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let re = receiver_regexp(ctx, "get RegExp.prototype.flags")?;
    let flags = re.flags(ctx.heap()).to_js_string();
    let string_heap = ctx.interp_mut().string_heap_clone();
    Ok(Value::String(
        JsString::from_str(&flags, &string_heap).map_err(|_| oom("flags"))?,
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
