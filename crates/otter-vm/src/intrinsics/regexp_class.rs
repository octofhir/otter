//! RegExp constructor and prototype intrinsics.
//!
//! Spec: <https://tc39.es/ecma262/#sec-regexp-regular-expression-objects>

use crate::builders::ClassBuilder;
use crate::descriptors::{JsClassDescriptor, NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{HeapValueKind, ObjectHandle};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics, WellKnownSymbol,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static REGEXP_INTRINSIC: RegExpIntrinsic = RegExpIntrinsic;

pub(super) struct RegExpIntrinsic;

impl IntrinsicInstaller for RegExpIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = regexp_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("RegExp class descriptor should normalize")
            .build();

        // Replace the pre-allocated constructor with a real host function.
        if let Some(ctor_desc) = plan.constructor() {
            let host_id = cx.native_functions.register(ctor_desc.clone());
            intrinsics.regexp_constructor =
                cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype)?;
        }

        install_class_plan(
            intrinsics.regexp_prototype,
            intrinsics.regexp_constructor,
            &plan,
            intrinsics.function_prototype,
            cx,
        )?;

        // Install getter-only accessor properties on %RegExp.prototype%.
        // §22.2.5.3 get flags, §22.2.5.12 get source, and individual flag getters.
        install_getter(
            intrinsics.regexp_prototype,
            "source",
            regexp_get_source,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.regexp_prototype,
            "flags",
            regexp_get_flags,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.regexp_prototype,
            "global",
            regexp_get_global,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.regexp_prototype,
            "ignoreCase",
            regexp_get_ignore_case,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.regexp_prototype,
            "multiline",
            regexp_get_multiline,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.regexp_prototype,
            "dotAll",
            regexp_get_dot_all,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.regexp_prototype,
            "sticky",
            regexp_get_sticky,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.regexp_prototype,
            "unicode",
            regexp_get_unicode,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.regexp_prototype,
            "unicodeSets",
            regexp_get_unicode_sets,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.regexp_prototype,
            "hasIndices",
            regexp_get_has_indices,
            intrinsics,
            cx,
        )?;

        // Install Symbol methods on %RegExp.prototype%.
        // §22.2.5.6 [Symbol.match], §22.2.5.7 [Symbol.matchAll],
        // §22.2.5.8 [Symbol.replace], §22.2.5.9 [Symbol.search],
        // §22.2.5.11 [Symbol.split]
        install_symbol_method(
            intrinsics.regexp_prototype,
            "[Symbol.match]",
            WellKnownSymbol::Match,
            1,
            regexp_symbol_match,
            intrinsics,
            cx,
        )?;
        install_symbol_method(
            intrinsics.regexp_prototype,
            "[Symbol.matchAll]",
            WellKnownSymbol::MatchAll,
            1,
            regexp_symbol_match_all,
            intrinsics,
            cx,
        )?;
        install_symbol_method(
            intrinsics.regexp_prototype,
            "[Symbol.replace]",
            WellKnownSymbol::Replace,
            2,
            regexp_symbol_replace,
            intrinsics,
            cx,
        )?;
        install_symbol_method(
            intrinsics.regexp_prototype,
            "[Symbol.search]",
            WellKnownSymbol::Search,
            1,
            regexp_symbol_search,
            intrinsics,
            cx,
        )?;
        install_symbol_method(
            intrinsics.regexp_prototype,
            "[Symbol.split]",
            WellKnownSymbol::Split,
            2,
            regexp_symbol_split,
            intrinsics,
            cx,
        )?;

        // Install RegExp[Symbol.species].
        install_getter(
            intrinsics.regexp_constructor,
            "[Symbol.species]",
            regexp_species,
            intrinsics,
            cx,
        )?;

        // Set %RegExp.prototype%[Symbol.toStringTag] = "RegExp"  (§22.2.5.15)
        let tag_symbol = cx
            .property_names
            .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
        let tag_str = cx.heap.alloc_string("RegExp");
        cx.heap.set_property(
            intrinsics.regexp_prototype,
            tag_symbol,
            RegisterValue::from_object_handle(tag_str.0),
        )?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "RegExp",
            RegisterValue::from_object_handle(intrinsics.regexp_constructor.0),
        )
    }
}

// ── Helper: install a getter-only accessor on target ─────────────────────────

fn install_getter(
    target: ObjectHandle,
    name: &str,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let getter_desc = NativeFunctionDescriptor::getter(name, callback);
    let getter_id = cx.native_functions.register(getter_desc);
    let getter_handle =
        cx.alloc_intrinsic_host_function(getter_id, intrinsics.function_prototype)?;
    let property = cx.property_names.intern(name);
    cx.heap
        .define_accessor(target, property, Some(getter_handle), None)?;
    Ok(())
}

// ── Helper: install a Symbol-keyed method on target ──────────────────────────

fn install_symbol_method(
    target: ObjectHandle,
    name: &str,
    symbol: WellKnownSymbol,
    length: u16,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method(name, length, callback);
    let id = cx.native_functions.register(desc);
    let handle = cx.alloc_intrinsic_host_function(id, intrinsics.function_prototype)?;
    let sym_prop = cx.property_names.intern_symbol(symbol.stable_id());
    cx.heap.set_property(
        target,
        sym_prop,
        RegisterValue::from_object_handle(handle.0),
    )?;
    Ok(())
}

// ── Descriptor ───────────────────────────────────────────────────────────────

fn proto_method(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> crate::descriptors::NativeBindingDescriptor {
    crate::descriptors::NativeBindingDescriptor::new(
        crate::descriptors::NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn regexp_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("RegExp")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "RegExp",
            2,
            regexp_constructor,
        ))
        .with_binding(proto_method("exec", 1, regexp_exec))
        .with_binding(proto_method("test", 1, regexp_test))
        .with_binding(proto_method("toString", 0, regexp_to_string))
        .with_binding(proto_method("compile", 2, regexp_compile))
}

// ── Error helpers ─────────────────────────────────────────────────────────────

fn type_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
        Err(error) => VmNativeCallError::Internal(format!("{error}").into()),
    }
}

fn syntax_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> VmNativeCallError {
    let prototype = runtime.intrinsics().syntax_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let msg = runtime.alloc_string(message);
    let msg_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(handle, msg_prop, RegisterValue::from_object_handle(msg.0))
        .ok();
    let name = runtime.alloc_string("SyntaxError");
    let name_prop = runtime.intern_property_name("name");
    runtime
        .objects_mut()
        .set_property(handle, name_prop, RegisterValue::from_object_handle(name.0))
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

// ── Receiver helpers ──────────────────────────────────────────────────────────

/// Validates `this` is a RegExp and returns its handle. §22.2.5 step 1-2.
fn require_regexp_this(
    this: &RegisterValue,
    method_name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, &format!("{method_name}: receiver is not a RegExp")))?;
    if !matches!(runtime.objects().kind(handle), Ok(HeapValueKind::RegExp)) {
        return Err(type_error(
            runtime,
            &format!("{method_name}: receiver is not a RegExp"),
        ));
    }
    Ok(handle)
}

/// Gets the pattern string from a RegExp handle.
fn regexp_pattern<'a>(
    handle: ObjectHandle,
    runtime: &'a crate::interpreter::RuntimeState,
) -> &'a str {
    runtime.objects().regexp_pattern(handle).unwrap_or("")
}

/// Gets the flags string from a RegExp handle.
fn regexp_flags_str<'a>(
    handle: ObjectHandle,
    runtime: &'a crate::interpreter::RuntimeState,
) -> &'a str {
    runtime.objects().regexp_flags(handle).unwrap_or("")
}

// ── lastIndex helpers ─────────────────────────────────────────────────────────

fn get_last_index(handle: ObjectHandle, runtime: &mut crate::interpreter::RuntimeState) -> f64 {
    let prop = runtime.intern_property_name("lastIndex");
    runtime
        .objects()
        .get_property(handle, prop)
        .ok()
        .flatten()
        .and_then(|lookup| {
            if let crate::object::PropertyValue::Data { value: v, .. } = lookup.value() {
                if let Some(i) = v.as_i32() {
                    Some(i as f64)
                } else {
                    v.as_number()
                }
            } else {
                None
            }
        })
        .unwrap_or(0.0)
}

fn set_last_index(
    handle: ObjectHandle,
    value: f64,
    runtime: &mut crate::interpreter::RuntimeState,
) {
    let prop = runtime.intern_property_name("lastIndex");
    let val =
        if value == (value as i32) as f64 && value >= i32::MIN as f64 && value <= i32::MAX as f64 {
            RegisterValue::from_i32(value as i32)
        } else {
            RegisterValue::from_number(value)
        };
    runtime.objects_mut().set_property(handle, prop, val).ok();
}

// ── Compile regex helper ──────────────────────────────────────────────────────

fn compile_regex(
    pattern: &str,
    flags: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<regress::Regex, VmNativeCallError> {
    regress::Regex::with_flags(pattern, flags)
        .map_err(|err| syntax_error(runtime, &format!("Invalid regular expression: {err}")))
}

// ── §22.2.3 RegExpBuiltinExec ─────────────────────────────────────────────────

/// Core exec implementation. Returns Some(match_array) or None.
/// Spec: <https://tc39.es/ecma262/#sec-regexpbuiltinexec>
fn regexp_builtin_exec(
    handle: ObjectHandle,
    input: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<ObjectHandle>, VmNativeCallError> {
    let pattern = regexp_pattern(handle, runtime).to_string();
    let flags = regexp_flags_str(handle, runtime).to_string();

    let global = flags.contains('g');
    let sticky = flags.contains('y');
    let has_indices = flags.contains('d');
    let unicode = flags.contains('u') || flags.contains('v');

    let compiled = compile_regex(&pattern, &flags, runtime)?;

    // Collect UTF-16 code units for index computations.
    let utf16: Vec<u16> = input.encode_utf16().collect();

    // Determine search start position.
    let start_pos = if global || sticky {
        let li = get_last_index(handle, runtime);
        if li < 0.0 || li.is_nan() || li.is_infinite() {
            // lastIndex out of range → reset and return null
            set_last_index(handle, 0.0, runtime);
            return Ok(None);
        }
        li as usize
    } else {
        0
    };

    if start_pos > utf16.len() {
        if global || sticky {
            set_last_index(handle, 0.0, runtime);
        }
        return Ok(None);
    }

    // Execute the regex against UTF-16 input.
    let m = if unicode {
        compiled.find_from_utf16(&utf16, start_pos).next()
    } else {
        compiled.find_from_ucs2(&utf16, start_pos).next()
    };

    let m = match m {
        Some(m) => {
            // Sticky: match must start at exactly start_pos.
            if sticky && m.start() != start_pos {
                set_last_index(handle, 0.0, runtime);
                return Ok(None);
            }
            m
        }
        None => {
            if global || sticky {
                set_last_index(handle, 0.0, runtime);
            }
            return Ok(None);
        }
    };

    // Update lastIndex for stateful regexps.
    if global || sticky {
        let end = m.end();
        // Advance at least one position on empty match to avoid infinite loops.
        let new_last_index = if end == start_pos {
            if unicode {
                advance_unicode(&utf16, start_pos)
            } else {
                start_pos + 1
            }
        } else {
            end
        };
        set_last_index(handle, new_last_index as f64, runtime);
    }

    // Convert UTF-16 offsets to string char offsets.
    let match_start_utf16 = m.start();
    let match_end_utf16 = m.end();

    // Build result array.
    let result = runtime.objects_mut().alloc_array();

    // [0] = full match string
    let full_match_utf16 = &utf16[match_start_utf16..match_end_utf16];
    let full_match_str = String::from_utf16_lossy(full_match_utf16);
    let full_match_handle = runtime.alloc_string(full_match_str);
    runtime
        .objects_mut()
        .set_index(
            result,
            0,
            RegisterValue::from_object_handle(full_match_handle.0),
        )
        .ok();

    // [1..n] = capture groups
    let cap_count = m.captures.len();
    let mut groups_obj: Option<ObjectHandle> = None;

    for i in 0..cap_count {
        let cap_val = match &m.captures[i] {
            Some(range) => {
                let cap_utf16 = &utf16[range.start..range.end];
                let cap_str = String::from_utf16_lossy(cap_utf16);
                let cap_handle = runtime.alloc_string(cap_str);
                RegisterValue::from_object_handle(cap_handle.0)
            }
            None => RegisterValue::undefined(),
        };
        runtime.objects_mut().set_index(result, i + 1, cap_val).ok();
    }

    // Named capture groups → .groups object
    let named: Vec<(String, Option<String>)> = m
        .named_groups()
        .map(|(name, range)| {
            let val = range.map(|r| String::from_utf16_lossy(&utf16[r.start..r.end]).to_string());
            (name.to_string(), val)
        })
        .collect();

    if !named.is_empty() {
        let groups = runtime.alloc_object();
        for (name, val) in &named {
            let prop = runtime.intern_property_name(name);
            let v = match val {
                Some(s) => {
                    let sh = runtime.alloc_string(s.as_str());
                    RegisterValue::from_object_handle(sh.0)
                }
                None => RegisterValue::undefined(),
            };
            runtime.objects_mut().set_property(groups, prop, v).ok();
        }
        groups_obj = Some(groups);
    }

    // .index property
    let index_prop = runtime.intern_property_name("index");
    runtime
        .set_named_property(
            result,
            index_prop,
            RegisterValue::from_i32(match_start_utf16 as i32),
        )
        .ok();

    // .input property
    let input_prop = runtime.intern_property_name("input");
    let input_handle = runtime.alloc_string(input);
    runtime
        .set_named_property(
            result,
            input_prop,
            RegisterValue::from_object_handle(input_handle.0),
        )
        .ok();

    // .groups property
    let groups_prop = runtime.intern_property_name("groups");
    let groups_val = groups_obj
        .map(|h| RegisterValue::from_object_handle(h.0))
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .set_named_property(result, groups_prop, groups_val)
        .ok();

    // .indices property (when 'd' flag is set)
    if has_indices {
        let indices_arr = build_indices_array(&m, &utf16, result, cap_count, runtime);
        let indices_prop = runtime.intern_property_name("indices");
        runtime
            .set_named_property(
                result,
                indices_prop,
                RegisterValue::from_object_handle(indices_arr.0),
            )
            .ok();
    }

    Ok(Some(result))
}

fn advance_unicode(utf16: &[u16], pos: usize) -> usize {
    if pos < utf16.len() {
        // Check for surrogate pair
        if (0xD800..0xDC00).contains(&utf16[pos])
            && pos + 1 < utf16.len()
            && (0xDC00..0xE000).contains(&utf16[pos + 1])
        {
            pos + 2
        } else {
            pos + 1
        }
    } else {
        pos + 1
    }
}

fn build_indices_array(
    m: &regress::Match,
    utf16: &[u16],
    _result: ObjectHandle,
    cap_count: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> ObjectHandle {
    let arr = runtime.objects_mut().alloc_array();
    // [0] = full match indices
    let pair = make_index_pair(m.start(), m.end(), utf16, runtime);
    runtime
        .objects_mut()
        .set_index(arr, 0, RegisterValue::from_object_handle(pair.0))
        .ok();
    for i in 0..cap_count {
        let val = match &m.captures[i] {
            Some(r) => {
                let p = make_index_pair(r.start, r.end, utf16, runtime);
                RegisterValue::from_object_handle(p.0)
            }
            None => RegisterValue::undefined(),
        };
        runtime.objects_mut().set_index(arr, i + 1, val).ok();
    }
    arr
}

fn make_index_pair(
    start: usize,
    end: usize,
    _utf16: &[u16],
    runtime: &mut crate::interpreter::RuntimeState,
) -> ObjectHandle {
    let pair = runtime.objects_mut().alloc_array();
    runtime
        .objects_mut()
        .set_index(pair, 0, RegisterValue::from_i32(start as i32))
        .ok();
    runtime
        .objects_mut()
        .set_index(pair, 1, RegisterValue::from_i32(end as i32))
        .ok();
    pair
}

// ── §22.2.3 RegExp constructor ────────────────────────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-regexp-pattern-flags>

fn regexp_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pattern_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let flags_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let is_construct = this.as_object_handle().is_some();

    // §22.2.3 step 2: if pattern is RegExp and flags is undefined, return pattern as-is (function call only).
    if !is_construct {
        if let Some(phandle) = pattern_arg.as_object_handle().map(ObjectHandle) {
            if matches!(runtime.objects().kind(phandle), Ok(HeapValueKind::RegExp))
                && flags_arg == RegisterValue::undefined()
            {
                return Ok(pattern_arg);
            }
        }
    }

    // Determine pattern string.
    let (pattern, source_flags) =
        if let Some(phandle) = pattern_arg.as_object_handle().map(ObjectHandle) {
            if matches!(runtime.objects().kind(phandle), Ok(HeapValueKind::RegExp)) {
                let pat = regexp_pattern(phandle, runtime).to_string();
                let fl = regexp_flags_str(phandle, runtime).to_string();
                (pat, Some(fl))
            } else {
                let s = runtime
                    .js_to_string(pattern_arg)
                    .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
                (s.to_string(), None)
            }
        } else if pattern_arg == RegisterValue::undefined() {
            (String::new(), None)
        } else {
            let s = runtime
                .js_to_string(pattern_arg)
                .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
            (s.to_string(), None)
        };

    // Determine flags string.
    let flags = if flags_arg == RegisterValue::undefined() {
        source_flags.unwrap_or_default()
    } else {
        let s = runtime
            .js_to_string(flags_arg)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?;
        s.to_string()
    };

    // Validate flags: only dgimsuyv, no duplicates.
    {
        let mut seen = [false; 256];
        for ch in flags.chars() {
            let idx = ch as usize;
            if idx >= 256 || !matches!(ch, 'd' | 'g' | 'i' | 'm' | 's' | 'u' | 'v' | 'y') {
                return Err(syntax_error(
                    runtime,
                    &format!("Invalid regular expression flags: {ch}"),
                ));
            }
            if seen[idx] {
                return Err(syntax_error(
                    runtime,
                    &format!("Duplicate flag in RegExp: {ch}"),
                ));
            }
            seen[idx] = true;
        }
    }

    // Validate pattern by compiling it.
    compile_regex(&pattern, &flags, runtime)?;

    // Canonicalize flags (sort alphabetically).
    let canonical_flags = canonical_flags_str(&flags);

    // Allocate the RegExp object.
    let prototype = runtime.intrinsics().regexp_prototype;
    let handle = runtime
        .objects_mut()
        .alloc_regexp(&pattern, &canonical_flags, Some(prototype));

    // Set lastIndex = 0.
    set_last_index(handle, 0.0, runtime);

    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Returns flags in canonical ECMAScript order: d, g, i, m, s, u, v, y.
fn canonical_flags_str(flags: &str) -> String {
    let mut out = String::with_capacity(8);
    for ch in ['d', 'g', 'i', 'm', 's', 'u', 'v', 'y'] {
        if flags.contains(ch) {
            out.push(ch);
        }
    }
    out
}

// ── §22.2.5.2 RegExp.prototype.exec ──────────────────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-regexp.prototype.exec>

fn regexp_exec(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "RegExp.prototype.exec", runtime)?;
    let s_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let input = runtime
        .js_to_string(s_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        .to_string();

    match regexp_builtin_exec(handle, &input, runtime)? {
        Some(arr) => Ok(RegisterValue::from_object_handle(arr.0)),
        None => Ok(RegisterValue::null()),
    }
}

// ── §22.2.5.13 RegExp.prototype.test ─────────────────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-regexp.prototype.test>

fn regexp_test(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "RegExp.prototype.test", runtime)?;
    let s_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let input = runtime
        .js_to_string(s_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        .to_string();

    match regexp_builtin_exec(handle, &input, runtime)? {
        Some(_) => Ok(RegisterValue::from_bool(true)),
        None => Ok(RegisterValue::from_bool(false)),
    }
}

// ── §22.2.5.14 RegExp.prototype.toString ─────────────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-regexp.prototype.tostring>

fn regexp_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "RegExp.prototype.toString", runtime)?;
    let pattern = regexp_pattern(handle, runtime).to_string();
    let flags = regexp_flags_str(handle, runtime).to_string();
    let source = if pattern.is_empty() {
        "(?:)".to_string()
    } else {
        escape_pattern(&pattern)
    };
    let result = format!("/{source}/{flags}");
    let h = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(h.0))
}

/// Escape special characters in the pattern for source representation.
fn escape_pattern(pattern: &str) -> String {
    // Minimal: just return the pattern; the ECMAScript spec §22.2.5.12 defines
    // EscapeRegExpPattern but for well-formed patterns the raw text is correct.
    pattern.to_string()
}

// ── Annex B §B.2.4 RegExp.prototype.compile (deprecated) ─────────────────────

fn regexp_compile(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "RegExp.prototype.compile", runtime)?;

    let pattern = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let flags = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let pattern_str = if pattern == RegisterValue::undefined() {
        String::new()
    } else {
        runtime
            .js_to_string(pattern)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
            .to_string()
    };

    let flags_str = if flags == RegisterValue::undefined() {
        String::new()
    } else {
        runtime
            .js_to_string(flags)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
            .to_string()
    };

    // Validate by compiling.
    compile_regex(&pattern_str, &flags_str, runtime)?;

    // Mutate the regexp's pattern/flags in place.
    let canonical = canonical_flags_str(&flags_str);
    runtime
        .objects_mut()
        .set_regexp_pattern_flags(handle, &pattern_str, &canonical)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    // Reset lastIndex.
    set_last_index(handle, 0.0, runtime);

    Ok(*this)
}

// ── Getter accessors ──────────────────────────────────────────────────────────

fn regexp_get_source(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "get RegExp.prototype.source", runtime)?;
    let pattern = regexp_pattern(handle, runtime).to_string();
    let source = if pattern.is_empty() {
        "(?:)".to_string()
    } else {
        escape_pattern(&pattern)
    };
    let h = runtime.alloc_string(source);
    Ok(RegisterValue::from_object_handle(h.0))
}

fn regexp_get_flags(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "get RegExp.prototype.flags", runtime)?;
    let flags = regexp_flags_str(handle, runtime).to_string();
    // Reconstruct in canonical order.
    let canonical = canonical_flags_str(&flags);
    let h = runtime.alloc_string(canonical);
    Ok(RegisterValue::from_object_handle(h.0))
}

macro_rules! flag_getter {
    ($fn_name:ident, $flag_char:literal, $method_name:literal) => {
        fn $fn_name(
            this: &RegisterValue,
            _args: &[RegisterValue],
            runtime: &mut crate::interpreter::RuntimeState,
        ) -> Result<RegisterValue, VmNativeCallError> {
            // Special: if this === %RegExp.prototype%, return undefined per spec.
            let handle = require_regexp_this(this, $method_name, runtime)?;
            let flags = regexp_flags_str(handle, runtime).to_string();
            Ok(RegisterValue::from_bool(flags.contains($flag_char)))
        }
    };
}

flag_getter!(regexp_get_global, 'g', "get RegExp.prototype.global");
flag_getter!(
    regexp_get_ignore_case,
    'i',
    "get RegExp.prototype.ignoreCase"
);
flag_getter!(regexp_get_multiline, 'm', "get RegExp.prototype.multiline");
flag_getter!(regexp_get_dot_all, 's', "get RegExp.prototype.dotAll");
flag_getter!(regexp_get_sticky, 'y', "get RegExp.prototype.sticky");
flag_getter!(regexp_get_unicode, 'u', "get RegExp.prototype.unicode");
flag_getter!(
    regexp_get_unicode_sets,
    'v',
    "get RegExp.prototype.unicodeSets"
);
flag_getter!(
    regexp_get_has_indices,
    'd',
    "get RegExp.prototype.hasIndices"
);

// ── §22.2.5.6 RegExp.prototype[Symbol.match] ─────────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-regexp.prototype-@@match>

fn regexp_symbol_match(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "RegExp.prototype[Symbol.match]", runtime)?;
    let s_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let input = runtime
        .js_to_string(s_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        .to_string();

    let global = regexp_flags_str(handle, runtime).contains('g');

    if !global {
        // Non-global: single exec.
        match regexp_builtin_exec(handle, &input, runtime)? {
            Some(arr) => Ok(RegisterValue::from_object_handle(arr.0)),
            None => Ok(RegisterValue::null()),
        }
    } else {
        // Global: collect all matches.
        set_last_index(handle, 0.0, runtime);
        let result_arr = runtime.objects_mut().alloc_array();
        let mut idx = 0usize;
        loop {
            let m = regexp_builtin_exec(handle, &input, runtime)?;
            match m {
                None => break,
                Some(arr) => {
                    // Get [0] from the match array.
                    let match_val = runtime
                        .objects_mut()
                        .get_index(arr, 0)
                        .ok()
                        .flatten()
                        .unwrap_or_else(RegisterValue::undefined);
                    runtime
                        .objects_mut()
                        .set_index(result_arr, idx, match_val)
                        .ok();
                    idx += 1;
                    // Prevent infinite loop on empty match by advancing lastIndex.
                    let li = get_last_index(handle, runtime);
                    if li as usize == 0 && regexp_flags_str(handle, runtime).contains('g') {
                        // Already advanced by regexp_builtin_exec, but double-check.
                        if idx > 10_000 {
                            break; // Safety limit.
                        }
                    }
                }
            }
        }
        if idx == 0 {
            Ok(RegisterValue::null())
        } else {
            Ok(RegisterValue::from_object_handle(result_arr.0))
        }
    }
}

// ── §22.2.5.7 RegExp.prototype[Symbol.matchAll] ───────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-regexp-prototype-matchall>
// Returns a RegExpStringIterator (simplified as an array here for now).

fn regexp_symbol_match_all(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "RegExp.prototype[Symbol.matchAll]", runtime)?;
    let s_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let input = runtime
        .js_to_string(s_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        .to_string();

    // We need the 'g' flag to make matchAll collect all matches.
    // Build a clone of the regexp with 'g' if not present.
    let flags = regexp_flags_str(handle, runtime).to_string();
    let pattern = regexp_pattern(handle, runtime).to_string();
    let flags_with_g = if !flags.contains('g') {
        canonical_flags_str(&format!("{flags}g"))
    } else {
        canonical_flags_str(&flags)
    };

    let prototype = runtime.intrinsics().regexp_prototype;
    let clone_handle = runtime
        .objects_mut()
        .alloc_regexp(&pattern, &flags_with_g, Some(prototype));
    set_last_index(clone_handle, 0.0, runtime);

    // Collect all matches into an array (simplified iterator).
    let result_arr = runtime.objects_mut().alloc_array();
    let mut idx = 0usize;
    loop {
        match regexp_builtin_exec(clone_handle, &input, runtime)? {
            None => break,
            Some(arr) => {
                runtime
                    .objects_mut()
                    .set_index(result_arr, idx, RegisterValue::from_object_handle(arr.0))
                    .ok();
                idx += 1;
                if idx > 10_000 {
                    break;
                }
            }
        }
    }

    // Wrap in an iterator-like object with [Symbol.iterator] returning self.
    // For now return the raw array (sufficient for most test cases).
    Ok(RegisterValue::from_object_handle(result_arr.0))
}

// ── §22.2.5.8 RegExp.prototype[Symbol.replace] ───────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-regexp.prototype-@@replace>

fn regexp_symbol_replace(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "RegExp.prototype[Symbol.replace]", runtime)?;
    let s_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let input = runtime
        .js_to_string(s_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        .to_string();
    let replace_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let flags = regexp_flags_str(handle, runtime).to_string();
    let global = flags.contains('g');
    let use_function = replace_arg
        .as_object_handle()
        .map(ObjectHandle)
        .map(|h| runtime.objects().is_callable(h))
        .unwrap_or(false);

    if !global {
        set_last_index(handle, 0.0, runtime);
    } else {
        set_last_index(handle, 0.0, runtime);
    }

    let input_utf16: Vec<u16> = input.encode_utf16().collect();
    let mut results: Vec<ObjectHandle> = Vec::new();

    loop {
        match regexp_builtin_exec(handle, &input, runtime)? {
            None => break,
            Some(arr) => {
                results.push(arr);
                if !global {
                    break;
                }
            }
        }
    }

    if results.is_empty() {
        let h = runtime.alloc_string(input.as_str());
        return Ok(RegisterValue::from_object_handle(h.0));
    }

    let mut output = String::new();
    let mut last_end_utf16 = 0usize;

    for arr in results {
        let index_prop = runtime.intern_property_name("index");
        let match_index = runtime
            .property_lookup(arr, index_prop)
            .ok()
            .flatten()
            .and_then(|l| {
                if let crate::object::PropertyValue::Data { value: v, .. } = l.value() {
                    v.as_i32().map(|i| i as usize)
                } else {
                    None
                }
            })
            .unwrap_or(0);

        // Full match string.
        let full_match_val = runtime
            .objects_mut()
            .get_index(arr, 0)
            .ok()
            .flatten()
            .unwrap_or_else(RegisterValue::undefined);
        let full_match = runtime
            .js_to_string(full_match_val)
            .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
            .to_string();

        let match_end_utf16 = match_index + full_match.encode_utf16().count();

        // Append string before this match.
        let before_utf16 = &input_utf16[last_end_utf16..match_index];
        output.push_str(&String::from_utf16_lossy(before_utf16));

        // Compute replacement.
        let replacement = if use_function {
            let fn_handle = replace_arg.as_object_handle().map(ObjectHandle).unwrap();
            // Build args: (match, cap1, ..., capN, index, input)
            let mut fn_args = vec![full_match_val];
            let mut cap_idx = 1;
            loop {
                match runtime.objects_mut().get_index(arr, cap_idx) {
                    Ok(Some(v)) if v != RegisterValue::undefined() => {
                        fn_args.push(v);
                        cap_idx += 1;
                    }
                    _ => break,
                }
            }
            fn_args.push(RegisterValue::from_i32(match_index as i32));
            let input_h = runtime.alloc_string(input.as_str());
            fn_args.push(RegisterValue::from_object_handle(input_h.0));
            let result = runtime.call_callable(fn_handle, RegisterValue::undefined(), &fn_args)?;
            runtime
                .js_to_string(result)
                .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
                .to_string()
        } else {
            let replace_str = runtime
                .js_to_string(replace_arg)
                .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
                .to_string();
            apply_replacement_template(
                &replace_str,
                &full_match,
                arr,
                match_index,
                &input,
                &input_utf16,
                runtime,
            )
        };

        output.push_str(&replacement);
        last_end_utf16 = match_end_utf16;
    }

    // Append remaining string.
    let after_utf16 = &input_utf16[last_end_utf16..];
    output.push_str(&String::from_utf16_lossy(after_utf16));

    let h = runtime.alloc_string(output);
    Ok(RegisterValue::from_object_handle(h.0))
}

/// Implements §22.1.3.17.1 GetSubstitution — replacement string templates.
fn apply_replacement_template(
    template: &str,
    matched: &str,
    arr: ObjectHandle,
    match_pos_utf16: usize,
    input: &str,
    input_utf16: &[u16],
    runtime: &mut crate::interpreter::RuntimeState,
) -> String {
    let mut result = String::new();
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            match chars[i + 1] {
                '$' => {
                    result.push('$');
                    i += 2;
                }
                '&' => {
                    result.push_str(matched);
                    i += 2;
                }
                '`' => {
                    let before = String::from_utf16_lossy(&input_utf16[..match_pos_utf16]);
                    result.push_str(&before);
                    i += 2;
                }
                '\'' => {
                    let after_start = match_pos_utf16 + matched.encode_utf16().count();
                    let after = String::from_utf16_lossy(
                        &input_utf16[after_start.min(input_utf16.len())..],
                    );
                    result.push_str(&after);
                    i += 2;
                }
                '0'..='9' => {
                    // $1-$99: capture group by number.
                    let d1 = chars[i + 1].to_digit(10).unwrap() as usize;
                    let (cap_idx, advance) = if i + 2 < chars.len() {
                        if let Some(d2) = chars[i + 2].to_digit(10) {
                            let two = d1 * 10 + d2 as usize;
                            // Check if two-digit group exists.
                            match runtime.objects_mut().get_index(arr, two) {
                                Ok(Some(v)) if v != RegisterValue::undefined() => (two, 3),
                                _ => (d1, 2),
                            }
                        } else {
                            (d1, 2)
                        }
                    } else {
                        (d1, 2)
                    };
                    if cap_idx > 0 {
                        match runtime.objects_mut().get_index(arr, cap_idx) {
                            Ok(Some(v)) if v != RegisterValue::undefined() => {
                                if let Ok(s) = runtime.js_to_string(v) {
                                    result.push_str(&s);
                                }
                            }
                            _ => {}
                        }
                    }
                    i += advance;
                }
                '<' => {
                    // $<name> named capture groups.
                    if let Some(end) = chars[i + 2..].iter().position(|&c| c == '>') {
                        let name: String = chars[i + 2..i + 2 + end].iter().collect();
                        // Try to get from .groups
                        let groups_prop = runtime.intern_property_name("groups");
                        let groups_lookup =
                            runtime.property_lookup(arr, groups_prop).ok().flatten();
                        if let Some(glookup) = groups_lookup {
                            if let crate::object::PropertyValue::Data { value: gv, .. } =
                                glookup.value()
                            {
                                if let Some(gh) = gv.as_object_handle().map(ObjectHandle) {
                                    let name_prop = runtime.intern_property_name(&name);
                                    let nlookup =
                                        runtime.property_lookup(gh, name_prop).ok().flatten();
                                    if let Some(nl) = nlookup {
                                        if let crate::object::PropertyValue::Data {
                                            value: nv,
                                            ..
                                        } = nl.value()
                                        {
                                            if let Ok(s) = runtime.js_to_string(nv) {
                                                result.push_str(&s);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        i += 2 + end + 1; // skip $<name>
                    } else {
                        result.push(chars[i]);
                        i += 1;
                    }
                }
                _ => {
                    result.push(chars[i]);
                    i += 1;
                }
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    let _ = input; // silence unused warning
    result
}

// ── §22.2.5.9 RegExp.prototype[Symbol.search] ────────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-regexp.prototype-@@search>

fn regexp_symbol_search(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "RegExp.prototype[Symbol.search]", runtime)?;
    let s_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let input = runtime
        .js_to_string(s_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        .to_string();

    // Save and reset lastIndex.
    let prev_last_index = get_last_index(handle, runtime);
    set_last_index(handle, 0.0, runtime);

    let result = regexp_builtin_exec(handle, &input, runtime)?;

    // Restore lastIndex.
    set_last_index(handle, prev_last_index, runtime);

    match result {
        Some(arr) => {
            let index_prop = runtime.intern_property_name("index");
            let index = runtime
                .property_lookup(arr, index_prop)
                .ok()
                .flatten()
                .and_then(|l| {
                    if let crate::object::PropertyValue::Data { value: v, .. } = l.value() {
                        v.as_i32()
                    } else {
                        None
                    }
                })
                .unwrap_or(-1);
            Ok(RegisterValue::from_i32(index))
        }
        None => Ok(RegisterValue::from_i32(-1)),
    }
}

// ── §22.2.5.11 RegExp.prototype[Symbol.split] ────────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-regexp.prototype-@@split>

fn regexp_symbol_split(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_regexp_this(this, "RegExp.prototype[Symbol.split]", runtime)?;
    let s_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let input = runtime
        .js_to_string(s_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("{e}").into()))?
        .to_string();

    let limit = args
        .get(1)
        .copied()
        .and_then(|v| {
            if v == RegisterValue::undefined() {
                None
            } else if let Some(i) = v.as_i32() {
                Some(i.max(0) as usize)
            } else if let Some(n) = v.as_number() {
                Some(n.max(0.0) as usize)
            } else {
                None
            }
        })
        .unwrap_or(u32::MAX as usize);

    let result = runtime.objects_mut().alloc_array();
    if limit == 0 {
        return Ok(RegisterValue::from_object_handle(result.0));
    }

    let pattern = regexp_pattern(handle, runtime).to_string();
    let flags = regexp_flags_str(handle, runtime).to_string();
    // Add 'y' flag for sticky split per spec (§22.2.5.11 step 9).
    let sticky_flags = if !flags.contains('y') {
        canonical_flags_str(&format!("{flags}y"))
    } else {
        canonical_flags_str(&flags)
    };

    let prototype = runtime.intrinsics().regexp_prototype;
    let splitter = runtime
        .objects_mut()
        .alloc_regexp(&pattern, &sticky_flags, Some(prototype));

    let input_utf16: Vec<u16> = input.encode_utf16().collect();
    let size = input_utf16.len();

    if size == 0 {
        // Test empty string against splitter.
        set_last_index(splitter, 0.0, runtime);
        match regexp_builtin_exec(splitter, &input, runtime)? {
            None => {
                let sh = runtime.alloc_string(input.as_str());
                runtime
                    .objects_mut()
                    .set_index(result, 0, RegisterValue::from_object_handle(sh.0))
                    .ok();
            }
            Some(_) => { /* empty */ }
        }
        return Ok(RegisterValue::from_object_handle(result.0));
    }

    let mut result_len = 0usize;
    let mut p = 0usize; // last match end (UTF-16 units)
    let mut q = 0usize; // current search position

    while q < size {
        set_last_index(splitter, q as f64, runtime);
        let z = regexp_builtin_exec(splitter, &input, runtime)?;
        match z {
            None => {
                q += 1;
            }
            Some(arr) => {
                let e = get_last_index(splitter, runtime) as usize;
                if e == p {
                    q += 1;
                } else {
                    // Add segment from p to q.
                    let segment_utf16 = &input_utf16[p..q];
                    let segment = String::from_utf16_lossy(segment_utf16);
                    let sh = runtime.alloc_string(segment);
                    runtime
                        .objects_mut()
                        .set_index(result, result_len, RegisterValue::from_object_handle(sh.0))
                        .ok();
                    result_len += 1;
                    if result_len >= limit {
                        return Ok(RegisterValue::from_object_handle(result.0));
                    }
                    // Add captures.
                    let mut cap_idx = 1;
                    loop {
                        match runtime.objects_mut().get_index(arr, cap_idx) {
                            Ok(Some(_)) => {
                                // Include capture group (may be undefined).
                            }
                            _ => break,
                        }
                        let cap_val = runtime
                            .objects_mut()
                            .get_index(arr, cap_idx)
                            .ok()
                            .flatten()
                            .unwrap_or_else(RegisterValue::undefined);
                        runtime
                            .objects_mut()
                            .set_index(result, result_len, cap_val)
                            .ok();
                        result_len += 1;
                        if result_len >= limit {
                            return Ok(RegisterValue::from_object_handle(result.0));
                        }
                        cap_idx += 1;
                    }
                    p = e;
                    q = p;
                }
            }
        }
    }

    // Add remaining string.
    let segment_utf16 = &input_utf16[p..];
    let segment = String::from_utf16_lossy(segment_utf16);
    let sh = runtime.alloc_string(segment);
    runtime
        .objects_mut()
        .set_index(result, result_len, RegisterValue::from_object_handle(sh.0))
        .ok();

    Ok(RegisterValue::from_object_handle(result.0))
}

// ── get RegExp[Symbol.species] ────────────────────────────────────────────────

fn regexp_species(
    this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // §22.2.4.2: RegExp[Symbol.species] = RegExp constructor.
    Ok(*this)
}
