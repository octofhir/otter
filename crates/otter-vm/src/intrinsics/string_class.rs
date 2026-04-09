use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics, WellKnownSymbol,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
    symbol_descriptive_string,
};

pub(super) static STRING_INTRINSIC: StringIntrinsic = StringIntrinsic;

const STRING_DATA_SLOT: &str = "__otter_string_data__";
const STRING_VALUE_OF_ERROR: &str = "String.prototype.valueOf requires a string receiver";
const STRING_INTERRUPT_POLL_INTERVAL: usize = 4096;
const MAX_INTRINSIC_STRING_BYTES: usize = 256 * 1024 * 1024;

#[inline]
fn check_interrupt_poll(
    runtime: &crate::interpreter::RuntimeState,
    index: usize,
) -> Result<(), VmNativeCallError> {
    if index % STRING_INTERRUPT_POLL_INTERVAL == 0 {
        runtime.check_interrupt()?;
    }
    Ok(())
}

pub(super) struct StringIntrinsic;

fn type_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> Result<VmNativeCallError, VmNativeCallError> {
    let error = runtime.alloc_type_error(message).map_err(|error| {
        VmNativeCallError::Internal(format!("TypeError allocation failed: {error}").into())
    })?;
    Ok(VmNativeCallError::Thrown(
        RegisterValue::from_object_handle(error.0),
    ))
}

impl IntrinsicInstaller for StringIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = string_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("String class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.string_constructor = constructor;
        install_class_plan(
            intrinsics.string_prototype(),
            intrinsics.string_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
        )?;
        initialize_string_prototype(intrinsics, cx)?;

        // Install String.prototype[Symbol.iterator].
        let iter_desc = NativeFunctionDescriptor::method("[Symbol.iterator]", 0, string_iterator);
        let iter_id = cx.native_functions.register(iter_desc);
        let iter_fn = cx.alloc_intrinsic_host_function(iter_id, intrinsics.function_prototype())?;
        let sym_iterator = cx
            .property_names
            .intern_symbol(super::WellKnownSymbol::Iterator.stable_id());
        cx.heap.set_property(
            intrinsics.string_prototype(),
            sym_iterator,
            RegisterValue::from_object_handle(iter_fn.0),
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
            "String",
            RegisterValue::from_object_handle(intrinsics.string_constructor().0),
        )
    }
}

fn proto(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn static_method(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Constructor,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn string_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("String")
        .with_constructor(
            NativeFunctionDescriptor::constructor("String", 1, string_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::StringPrototype),
        )
        // ── Static methods ──
        .with_binding(static_method("fromCharCode", 1, string_from_char_code))
        .with_binding(static_method("fromCodePoint", 1, string_from_code_point))
        .with_binding(static_method("raw", 1, string_raw))
        // ── Prototype methods ──
        .with_binding(proto("toString", 0, string_value_of))
        .with_binding(proto("valueOf", 0, string_value_of))
        .with_binding(proto("concat", 1, string_concat))
        .with_binding(proto("charAt", 1, string_char_at))
        .with_binding(proto("charCodeAt", 1, string_char_code_at))
        .with_binding(proto("codePointAt", 1, string_code_point_at))
        .with_binding(proto("indexOf", 1, string_index_of))
        .with_binding(proto("lastIndexOf", 1, string_last_index_of))
        .with_binding(proto("includes", 1, string_includes))
        .with_binding(proto("startsWith", 1, string_starts_with))
        .with_binding(proto("endsWith", 1, string_ends_with))
        .with_binding(proto("slice", 2, string_slice))
        .with_binding(proto("substring", 2, string_substring))
        .with_binding(proto("toUpperCase", 0, string_to_upper_case))
        .with_binding(proto("toLowerCase", 0, string_to_lower_case))
        .with_binding(proto("trim", 0, string_trim))
        .with_binding(proto("trimStart", 0, string_trim_start))
        .with_binding(proto("trimEnd", 0, string_trim_end))
        .with_binding(proto("repeat", 1, string_repeat))
        .with_binding(proto("padStart", 1, string_pad_start))
        .with_binding(proto("padEnd", 1, string_pad_end))
        .with_binding(proto("split", 2, string_split))
        .with_binding(proto("at", 1, string_at))
        .with_binding(proto("match", 1, string_match))
        .with_binding(proto("matchAll", 1, string_match_all))
        .with_binding(proto("search", 1, string_search))
        .with_binding(proto("replace", 2, string_replace))
        .with_binding(proto("replaceAll", 2, string_replace_all))
        .with_binding(proto("normalize", 0, string_normalize))
        .with_binding(proto("isWellFormed", 0, string_is_well_formed))
        .with_binding(proto("toWellFormed", 0, string_to_well_formed))
        .with_binding(proto("localeCompare", 1, string_locale_compare))
        .with_binding(proto("toLocaleLowerCase", 0, string_to_locale_lower_case))
        .with_binding(proto("toLocaleUpperCase", 0, string_to_locale_upper_case))
}

fn string_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let coerced = coerce_to_string(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;
    let primitive = runtime.alloc_string(coerced);

    if let Some(receiver) = this.as_object_handle().map(ObjectHandle) {
        set_string_data(receiver, primitive, runtime)?;
        Ok(*this)
    } else {
        Ok(RegisterValue::from_object_handle(primitive.0))
    }
}

fn string_value_of(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if let Some(handle) = this.as_object_handle().map(ObjectHandle) {
        if matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String)) {
            return Ok(*this);
        }
        if let Some(primitive) = string_data(handle, runtime)? {
            return Ok(RegisterValue::from_object_handle(primitive.0));
        }
    }

    Err(VmNativeCallError::Internal(STRING_VALUE_OF_ERROR.into()))
}

fn string_concat(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let mut text = runtime
        .js_to_string(*this)
        .map_err(|error| map_interpreter_error(error, runtime))?
        .into_string();
    for arg in args {
        let next = runtime
            .js_to_string(*arg)
            .map_err(|error| map_interpreter_error(error, runtime))?;
        text.push_str(&next);
    }
    let result = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(result.0))
}

fn coerce_to_string(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Box<str>, VmNativeCallError> {
    if value == RegisterValue::undefined() {
        return Ok("undefined".into());
    }
    if value == RegisterValue::null() {
        return Ok("null".into());
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(if boolean { "true" } else { "false" }.into());
    }
    if let Some(number) = value.as_number() {
        return Ok(number_to_string(number).into_boxed_str());
    }
    if value.is_symbol() {
        return Ok(symbol_descriptive_string(value, runtime).into_boxed_str());
    }
    if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
        if let Some(string) = runtime
            .objects()
            .string_value(handle)
            .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?
        {
            return Ok(string.to_string().into_boxed_str());
        }
        if let Some(primitive) = string_data(handle, runtime)?
            && let Some(string) = runtime
                .objects()
                .string_value(primitive)
                .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?
        {
            return Ok(string.to_string().into_boxed_str());
        }
        return runtime.js_to_string(value).map_err(|error| match error {
            crate::interpreter::InterpreterError::UncaughtThrow(value) => {
                VmNativeCallError::Thrown(value)
            }
            crate::interpreter::InterpreterError::TypeError(message) => {
                match type_error(runtime, &message) {
                    Ok(error) => error,
                    Err(error) => error,
                }
            }
            other => VmNativeCallError::Internal(format!("{other}").into()),
        });
    }

    Ok(String::new().into_boxed_str())
}

fn map_interpreter_error(
    error: crate::interpreter::InterpreterError,
    runtime: &mut crate::interpreter::RuntimeState,
) -> VmNativeCallError {
    match error {
        crate::interpreter::InterpreterError::UncaughtThrow(value) => {
            VmNativeCallError::Thrown(value)
        }
        crate::interpreter::InterpreterError::TypeError(message) => {
            match type_error(runtime, &message) {
                Ok(error) => error,
                Err(error) => error,
            }
        }
        other => VmNativeCallError::Internal(format!("{other}").into()),
    }
}

fn initialize_string_prototype(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let primitive = cx.heap.alloc_string("");
    cx.heap
        .set_prototype(primitive, Some(intrinsics.string_prototype()))?;
    let backing = cx.property_names.intern(STRING_DATA_SLOT);
    cx.heap.define_own_property(
        intrinsics.string_prototype(),
        backing,
        crate::object::PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(primitive.0),
            crate::object::PropertyAttributes::from_flags(true, false, true),
        ),
    )?;
    Ok(())
}

fn set_string_data(
    receiver: ObjectHandle,
    primitive: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let backing = runtime.intern_property_name(STRING_DATA_SLOT);
    runtime
        .objects_mut()
        .define_own_property(
            receiver,
            backing,
            crate::object::PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(primitive.0),
                crate::object::PropertyAttributes::from_flags(true, false, true),
            ),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("String constructor backing store failed: {error:?}").into(),
            )
        })?;
    Ok(())
}

fn string_data(
    handle: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<ObjectHandle>, VmNativeCallError> {
    let backing = runtime.intern_property_name(STRING_DATA_SLOT);
    let Some(lookup) = runtime
        .objects()
        .get_property(handle, backing)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("String data lookup failed: {error:?}").into())
        })?
    else {
        return Ok(None);
    };

    let PropertyValue::Data { value, .. } = lookup.value() else {
        return Ok(None);
    };

    Ok(value.as_object_handle().map(ObjectHandle))
}

pub(super) fn box_string_object(
    primitive: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let wrapper =
        runtime.alloc_object_with_prototype(Some(runtime.intrinsics().string_prototype()));
    set_string_data(wrapper, primitive, runtime)?;
    Ok(RegisterValue::from_object_handle(wrapper.0))
}

/// Extract the string content from `this` (primitive string handle or wrapper object).
fn this_string_value(
    this: &RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Box<str>, VmNativeCallError> {
    runtime
        .js_to_string(*this)
        .map_err(|error| map_interpreter_error(error, runtime))
}

/// Resolve an integer argument, defaulting to `default` if undefined/absent.
fn int_arg(args: &[RegisterValue], index: usize, default: i32) -> i32 {
    args.get(index)
        .copied()
        .and_then(|v| {
            if v == RegisterValue::undefined() {
                None
            } else {
                v.as_i32().or_else(|| v.as_number().map(|n| n as i32))
            }
        })
        .unwrap_or(default)
}

/// Clamp a relative index per ES spec (negative = from end).
fn relative_index(raw: i32, len: usize) -> usize {
    if raw < 0 {
        (len as i32 + raw).max(0) as usize
    } else {
        (raw as usize).min(len)
    }
}

// ── §22.1.3.1 String.prototype.at(index) ───────────────────────────────────

fn string_at(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i32;
    let index = int_arg(args, 0, 0);
    let actual = if index < 0 { len + index } else { index };
    if actual < 0 || actual >= len {
        return Ok(RegisterValue::undefined());
    }
    let ch = chars[actual as usize];
    let handle = runtime.alloc_string(ch.to_string());
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.2 String.prototype.charAt(pos) ─────────────────────────────────

fn string_char_at(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let pos = int_arg(args, 0, 0);
    if pos < 0 {
        let handle = runtime.alloc_string("");
        return Ok(RegisterValue::from_object_handle(handle.0));
    }
    match s.chars().nth(pos as usize) {
        Some(ch) => {
            let handle = runtime.alloc_string(ch.to_string());
            Ok(RegisterValue::from_object_handle(handle.0))
        }
        None => {
            let handle = runtime.alloc_string("");
            Ok(RegisterValue::from_object_handle(handle.0))
        }
    }
}

// ── §22.1.3.3 String.prototype.charCodeAt(pos) ─────────────────────────────

fn string_char_code_at(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let pos = int_arg(args, 0, 0);
    // ES uses UTF-16 code units; approximate with chars for BMP.
    let code_units: Vec<u16> = s.encode_utf16().collect();
    if pos < 0 || (pos as usize) >= code_units.len() {
        return Ok(RegisterValue::from_number(f64::NAN));
    }
    Ok(RegisterValue::from_i32(code_units[pos as usize] as i32))
}

// ── §22.1.3.4 String.prototype.codePointAt(pos) ────────────────────────────

fn string_code_point_at(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let pos = int_arg(args, 0, 0);
    let code_units: Vec<u16> = s.encode_utf16().collect();
    if pos < 0 || (pos as usize) >= code_units.len() {
        return Ok(RegisterValue::undefined());
    }
    let i = pos as usize;
    let first = code_units[i];
    if (0xD800..=0xDBFF).contains(&first) && i + 1 < code_units.len() {
        let second = code_units[i + 1];
        if (0xDC00..=0xDFFF).contains(&second) {
            let cp = 0x10000 + ((first as u32 - 0xD800) << 10) + (second as u32 - 0xDC00);
            return Ok(RegisterValue::from_i32(cp as i32));
        }
    }
    Ok(RegisterValue::from_i32(first as i32))
}

// ── §22.1.3.9 String.prototype.indexOf(searchString [, position]) ───────────

fn string_index_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let search = args
        .first()
        .copied()
        .map(|v| {
            runtime
                .js_to_string(v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?
        .unwrap_or_else(|| "undefined".into());
    let pos = int_arg(args, 1, 0).max(0) as usize;
    // UTF-16 based positions for spec compliance.
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let search_units: Vec<u16> = search.encode_utf16().collect();
    if search_units.is_empty() {
        return Ok(RegisterValue::from_i32(pos.min(s_units.len()) as i32));
    }
    for i in pos..s_units.len() {
        if s_units[i..].starts_with(&search_units) {
            return Ok(RegisterValue::from_i32(i as i32));
        }
    }
    Ok(RegisterValue::from_i32(-1))
}

// ── §22.1.3.10 String.prototype.lastIndexOf(searchString [, position]) ──────

fn string_last_index_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let search = args
        .first()
        .copied()
        .map(|v| {
            runtime
                .js_to_string(v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?
        .unwrap_or_else(|| "undefined".into());
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let search_units: Vec<u16> = search.encode_utf16().collect();
    let pos = args
        .get(1)
        .copied()
        .and_then(|v| {
            if v == RegisterValue::undefined() {
                None
            } else {
                v.as_number()
            }
        })
        .map(|n| {
            if n.is_nan() {
                s_units.len()
            } else {
                n.max(0.0) as usize
            }
        })
        .unwrap_or(s_units.len());

    if search_units.is_empty() {
        return Ok(RegisterValue::from_i32(pos.min(s_units.len()) as i32));
    }
    let limit = pos.min(s_units.len().saturating_sub(search_units.len()));
    for i in (0..=limit).rev() {
        if s_units[i..].starts_with(&search_units) {
            return Ok(RegisterValue::from_i32(i as i32));
        }
    }
    Ok(RegisterValue::from_i32(-1))
}

// ── §22.1.3.7 String.prototype.includes(searchString [, position]) ──────────

fn string_includes(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let search = args
        .first()
        .copied()
        .map(|v| {
            runtime
                .js_to_string(v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?
        .unwrap_or_else(|| "undefined".into());
    let pos = int_arg(args, 1, 0).max(0) as usize;
    // Work in UTF-16 positions.
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let search_units: Vec<u16> = search.encode_utf16().collect();
    if search_units.is_empty() {
        return Ok(RegisterValue::from_bool(true));
    }
    for i in pos..s_units.len() {
        if s_units[i..].starts_with(&search_units) {
            return Ok(RegisterValue::from_bool(true));
        }
    }
    Ok(RegisterValue::from_bool(false))
}

// ── §22.1.3.22 String.prototype.startsWith(searchString [, position]) ───────

fn string_starts_with(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let search = args
        .first()
        .copied()
        .map(|v| {
            runtime
                .js_to_string(v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?
        .unwrap_or_else(|| "undefined".into());
    let pos = int_arg(args, 1, 0).max(0) as usize;
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let search_units: Vec<u16> = search.encode_utf16().collect();
    if pos + search_units.len() > s_units.len() {
        return Ok(RegisterValue::from_bool(false));
    }
    Ok(RegisterValue::from_bool(
        s_units[pos..].starts_with(&search_units),
    ))
}

// ── §22.1.3.5 String.prototype.endsWith(searchString [, endPosition]) ───────

fn string_ends_with(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let search = args
        .first()
        .copied()
        .map(|v| {
            runtime
                .js_to_string(v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?
        .unwrap_or_else(|| "undefined".into());
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let search_units: Vec<u16> = search.encode_utf16().collect();
    let end_pos = args
        .get(1)
        .copied()
        .and_then(|v| {
            if v == RegisterValue::undefined() {
                None
            } else {
                v.as_i32()
            }
        })
        .map(|n| n.max(0) as usize)
        .unwrap_or(s_units.len())
        .min(s_units.len());
    if search_units.len() > end_pos {
        return Ok(RegisterValue::from_bool(false));
    }
    let start = end_pos - search_units.len();
    Ok(RegisterValue::from_bool(
        s_units[start..end_pos] == search_units[..],
    ))
}

// ── §22.1.3.21 String.prototype.slice(start, end) ──────────────────────────

fn string_slice(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let len = s_units.len();
    let start = relative_index(int_arg(args, 0, 0), len);
    let end = if args.get(1).copied() == Some(RegisterValue::undefined()) || args.get(1).is_none() {
        len
    } else {
        relative_index(int_arg(args, 1, len as i32), len)
    };
    if start >= end {
        let handle = runtime.alloc_string("");
        return Ok(RegisterValue::from_object_handle(handle.0));
    }
    let result = String::from_utf16_lossy(&s_units[start..end]);
    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.23 String.prototype.substring(start, end) ──────────────────────

fn string_substring(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let len = s_units.len() as i32;
    let raw_start = int_arg(args, 0, 0).clamp(0, len) as usize;
    let raw_end =
        if args.get(1).copied() == Some(RegisterValue::undefined()) || args.get(1).is_none() {
            s_units.len()
        } else {
            int_arg(args, 1, len).clamp(0, len) as usize
        };
    let (from, to) = if raw_start <= raw_end {
        (raw_start, raw_end)
    } else {
        (raw_end, raw_start)
    };
    let result = String::from_utf16_lossy(&s_units[from..to]);
    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.27 String.prototype.toUpperCase() ──────────────────────────────

fn string_to_upper_case(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let handle = runtime.alloc_string(s.to_uppercase());
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.26 String.prototype.toLowerCase() ──────────────────────────────

fn string_to_lower_case(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let handle = runtime.alloc_string(s.to_lowercase());
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.29 String.prototype.trim() ─────────────────────────────────────

fn string_trim(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let handle = runtime.alloc_string(s.trim());
    Ok(RegisterValue::from_object_handle(handle.0))
}

fn string_trim_start(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let handle = runtime.alloc_string(s.trim_start());
    Ok(RegisterValue::from_object_handle(handle.0))
}

fn string_trim_end(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let handle = runtime.alloc_string(s.trim_end());
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.16 String.prototype.repeat(count) ─────────────────────────────

fn string_repeat(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let count = int_arg(args, 0, 0);
    if count < 0 {
        return Err(range_error(runtime, "Invalid count value"));
    }
    let repeat_count = count as usize;
    let result_len = s
        .len()
        .checked_mul(repeat_count)
        .ok_or_else(|| range_error(runtime, "Invalid string length: repeat result is too large"))?;
    if result_len > MAX_INTRINSIC_STRING_BYTES {
        return Err(range_error(
            runtime,
            "Invalid string length: repeat result is too large",
        ));
    }
    let mut result = String::with_capacity(result_len);
    for i in 0..repeat_count {
        check_interrupt_poll(runtime, i)?;
        result.push_str(&s);
    }
    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

fn range_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    let prototype = runtime.intrinsics().range_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let msg = runtime.alloc_string(message);
    let msg_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(handle, msg_prop, RegisterValue::from_object_handle(msg.0))
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.14 String.prototype.padStart(maxLength [, fillString]) ─────────

fn string_pad_start(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let max_len = int_arg(args, 0, 0).max(0) as usize;
    if max_len > MAX_INTRINSIC_STRING_BYTES {
        return Err(range_error(
            runtime,
            "Invalid string length: padded string is too large",
        ));
    }
    let s_units: Vec<u16> = s.encode_utf16().collect();
    if s_units.len() >= max_len {
        let handle = runtime.alloc_string(&*s);
        return Ok(RegisterValue::from_object_handle(handle.0));
    }
    let fill = args
        .get(1)
        .copied()
        .filter(|v| *v != RegisterValue::undefined())
        .map(|v| {
            runtime
                .js_to_string(v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?
        .unwrap_or_else(|| " ".into());
    let fill_units: Vec<u16> = fill.encode_utf16().collect();
    if fill_units.is_empty() {
        let handle = runtime.alloc_string(&*s);
        return Ok(RegisterValue::from_object_handle(handle.0));
    }
    let pad_needed = max_len - s_units.len();
    let mut padded: Vec<u16> = Vec::with_capacity(max_len);
    for i in 0..pad_needed {
        check_interrupt_poll(runtime, i)?;
        padded.push(fill_units[i % fill_units.len()]);
    }
    padded.extend_from_slice(&s_units);
    let result = String::from_utf16_lossy(&padded);
    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.13 String.prototype.padEnd(maxLength [, fillString]) ───────────

fn string_pad_end(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let max_len = int_arg(args, 0, 0).max(0) as usize;
    if max_len > MAX_INTRINSIC_STRING_BYTES {
        return Err(range_error(
            runtime,
            "Invalid string length: padded string is too large",
        ));
    }
    let s_units: Vec<u16> = s.encode_utf16().collect();
    if s_units.len() >= max_len {
        let handle = runtime.alloc_string(&*s);
        return Ok(RegisterValue::from_object_handle(handle.0));
    }
    let fill = args
        .get(1)
        .copied()
        .filter(|v| *v != RegisterValue::undefined())
        .map(|v| {
            runtime
                .js_to_string(v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?
        .unwrap_or_else(|| " ".into());
    let fill_units: Vec<u16> = fill.encode_utf16().collect();
    if fill_units.is_empty() {
        let handle = runtime.alloc_string(&*s);
        return Ok(RegisterValue::from_object_handle(handle.0));
    }
    let pad_needed = max_len - s_units.len();
    let mut padded: Vec<u16> = Vec::with_capacity(max_len);
    padded.extend_from_slice(&s_units);
    for i in 0..pad_needed {
        check_interrupt_poll(runtime, i)?;
        padded.push(fill_units[i % fill_units.len()]);
    }
    let result = String::from_utf16_lossy(&padded);
    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Try to call `arg[symbolKey](this, ...extra_args)`.
/// Returns `Some(result)` if the symbol method exists and is callable, `None` otherwise.
fn try_symbol_dispatch(
    symbol: WellKnownSymbol,
    arg: RegisterValue,
    this: &RegisterValue,
    extra_args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    let handle = match arg.as_object_handle().map(ObjectHandle) {
        Some(h) => h,
        None => return Ok(None),
    };
    let sym_prop = runtime.intern_symbol_property_name(symbol.stable_id());
    let method_val = runtime.ordinary_get(handle, sym_prop, arg)?;
    if method_val == RegisterValue::undefined() || method_val == RegisterValue::null() {
        return Ok(None);
    }
    let method_handle = match method_val.as_object_handle().map(ObjectHandle) {
        Some(h) if runtime.objects().is_callable(h) => h,
        _ => return Ok(None),
    };
    let mut call_args = Vec::with_capacity(1 + extra_args.len());
    call_args.push(*this);
    call_args.extend_from_slice(extra_args);
    let result = runtime.call_callable(method_handle, arg, &call_args)?;
    Ok(Some(result))
}

// ── §22.1.3.12 String.prototype.match(regexp) ────────────────────────────

fn string_match(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let regexp_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // Step 1-2: If regexp has Symbol.match, call regexp[Symbol.match](this).
    if let Some(result) =
        try_symbol_dispatch(WellKnownSymbol::Match, regexp_arg, this, &[], runtime)?
    {
        return Ok(result);
    }

    // Step 3-4: Coerce to string, create a RegExp, then delegate via Symbol.match.
    let search_str = runtime
        .js_to_string(regexp_arg)
        .map_err(|e| map_interpreter_error(e, runtime))?;
    let prototype = runtime.intrinsics().regexp_prototype;
    let re = runtime.alloc_regexp(&search_str, "", Some(prototype));
    let re_val = RegisterValue::from_object_handle(re.0);
    if let Some(result) = try_symbol_dispatch(WellKnownSymbol::Match, re_val, this, &[], runtime)? {
        return Ok(result);
    }
    Ok(RegisterValue::null())
}

// ── §22.1.3.13 String.prototype.matchAll(regexp) ─────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-string.prototype.matchall>
// Delegates through @@matchAll when present and enforces the global-flag check
// for built-in RegExp objects before dispatch.

fn string_match_all(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let regexp_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    if let Some(handle) = regexp_arg.as_object_handle().map(ObjectHandle)
        && matches!(runtime.objects().kind(handle), Ok(HeapValueKind::RegExp))
    {
        let flags_property = runtime.intern_property_name("flags");
        let flags_value = runtime.ordinary_get(handle, flags_property, regexp_arg)?;
        let flags = runtime
            .js_to_string(flags_value)
            .map_err(|e| map_interpreter_error(e, runtime))?;
        if !flags.contains('g') {
            return Err(type_error(
                runtime,
                "String.prototype.matchAll requires a global RegExp argument",
            )?);
        }
    }

    if let Some(result) =
        try_symbol_dispatch(WellKnownSymbol::MatchAll, regexp_arg, this, &[], runtime)?
    {
        return Ok(result);
    }

    let pattern = if regexp_arg == RegisterValue::undefined() {
        String::new()
    } else {
        runtime
            .js_to_string(regexp_arg)
            .map_err(|e| map_interpreter_error(e, runtime))?
            .to_string()
    };
    let prototype = runtime.intrinsics().regexp_prototype;
    let re = runtime.alloc_regexp(&pattern, "g", Some(prototype));
    let re_val = RegisterValue::from_object_handle(re.0);
    if let Some(result) =
        try_symbol_dispatch(WellKnownSymbol::MatchAll, re_val, this, &[], runtime)?
    {
        return Ok(result);
    }
    Ok(RegisterValue::null())
}

// ── §22.1.3.21 String.prototype.search(regexp) ───────────────────────────

fn string_search(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let regexp_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // Step 1-2: If regexp has Symbol.search, call regexp[Symbol.search](this).
    if let Some(result) =
        try_symbol_dispatch(WellKnownSymbol::Search, regexp_arg, this, &[], runtime)?
    {
        return Ok(result);
    }

    // Step 3-4: Coerce to string, create a RegExp, then delegate via Symbol.search.
    let search_str = runtime
        .js_to_string(regexp_arg)
        .map_err(|e| map_interpreter_error(e, runtime))?;
    let prototype = runtime.intrinsics().regexp_prototype;
    let re = runtime.alloc_regexp(&search_str, "", Some(prototype));
    let re_val = RegisterValue::from_object_handle(re.0);
    if let Some(result) = try_symbol_dispatch(WellKnownSymbol::Search, re_val, this, &[], runtime)?
    {
        return Ok(result);
    }
    Ok(RegisterValue::from_i32(-1))
}

// ── §22.1.3.20 String.prototype.split(separator, limit) ────────────────────

fn string_split(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // Step 2: If separator has Symbol.split, delegate.
    let separator_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let limit_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    if let Some(result) = try_symbol_dispatch(
        WellKnownSymbol::Split,
        separator_arg,
        this,
        &[limit_arg],
        runtime,
    )? {
        return Ok(result);
    }

    let s = this_string_value(this, runtime)?;
    let limit = args
        .get(1)
        .copied()
        .and_then(|v| {
            if v == RegisterValue::undefined() {
                None
            } else {
                v.as_i32()
                    .map(|n| n.max(0) as usize)
                    .or_else(|| v.as_number().map(|n| n.max(0.0) as usize))
            }
        })
        .unwrap_or(u32::MAX as usize);

    let separator = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    if separator == RegisterValue::undefined() {
        let result = runtime.alloc_array();
        let handle = runtime.alloc_string(&*s);
        runtime
            .objects_mut()
            .set_index(result, 0, RegisterValue::from_object_handle(handle.0))
            .ok();
        return Ok(RegisterValue::from_object_handle(result.0));
    }
    let sep = runtime
        .js_to_string(separator)
        .map_err(|e| map_interpreter_error(e, runtime))?;

    let result = runtime.alloc_array();
    if limit == 0 {
        return Ok(RegisterValue::from_object_handle(result.0));
    }

    if sep.is_empty() {
        // Split into individual characters.
        let chars: Vec<char> = s.chars().collect();
        for (i, ch) in chars.iter().enumerate() {
            check_interrupt_poll(runtime, i)?;
            if i >= limit {
                break;
            }
            let handle = runtime.alloc_string(ch.to_string());
            runtime
                .objects_mut()
                .set_index(result, i, RegisterValue::from_object_handle(handle.0))
                .ok();
        }
        return Ok(RegisterValue::from_object_handle(result.0));
    }

    let mut count = 0usize;
    let mut start = 0usize;
    let s_bytes = s.as_bytes();
    let sep_bytes = sep.as_bytes();
    while start <= s_bytes.len() && count < limit {
        check_interrupt_poll(runtime, count)?;
        match s_bytes[start..]
            .windows(sep_bytes.len())
            .position(|w| w == sep_bytes)
        {
            Some(pos) => {
                let piece = &s[start..start + pos];
                let handle = runtime.alloc_string(piece);
                runtime
                    .objects_mut()
                    .set_index(result, count, RegisterValue::from_object_handle(handle.0))
                    .ok();
                count += 1;
                start = start + pos + sep_bytes.len();
            }
            None => {
                break;
            }
        }
    }
    if count < limit {
        let piece = &s[start..];
        let handle = runtime.alloc_string(piece);
        runtime
            .objects_mut()
            .set_index(result, count, RegisterValue::from_object_handle(handle.0))
            .ok();
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

// ── §22.1.3.17 String.prototype.replace(searchValue, replaceValue) ─────────

fn string_replace(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // Step 2: If searchValue has Symbol.replace, delegate.
    let search_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let replace_arg_raw = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    if let Some(result) = try_symbol_dispatch(
        WellKnownSymbol::Replace,
        search_arg,
        this,
        &[replace_arg_raw],
        runtime,
    )? {
        return Ok(result);
    }

    let s = this_string_value(this, runtime)?;
    let search = runtime
        .js_to_string(search_arg)
        .map_err(|e| map_interpreter_error(e, runtime))?;

    let replace_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let is_fn = replace_arg
        .as_object_handle()
        .map(ObjectHandle)
        .map(|h| runtime.objects().is_callable(h))
        .unwrap_or(false);

    if let Some(pos) = s.find(&*search) {
        let replacement = if is_fn {
            let callback = replace_arg.as_object_handle().map(ObjectHandle).unwrap();
            let match_str = runtime.alloc_string(&*search);
            let result = runtime.call_callable(
                callback,
                RegisterValue::undefined(),
                &[
                    RegisterValue::from_object_handle(match_str.0),
                    RegisterValue::from_i32(pos as i32),
                    *this,
                ],
            )?;
            runtime
                .js_to_string(result)
                .map_err(|e| map_interpreter_error(e, runtime))?
        } else {
            runtime
                .js_to_string(replace_arg)
                .map_err(|e| map_interpreter_error(e, runtime))?
        };
        let mut result = String::with_capacity(s.len());
        result.push_str(&s[..pos]);
        result.push_str(&replacement);
        result.push_str(&s[pos + search.len()..]);
        let handle = runtime.alloc_string(result);
        Ok(RegisterValue::from_object_handle(handle.0))
    } else {
        let handle = runtime.alloc_string(&*s);
        Ok(RegisterValue::from_object_handle(handle.0))
    }
}

// ── §22.1.3.18 String.prototype.replaceAll(searchValue, replaceValue) ──────

fn string_replace_all(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;
    let search = args
        .first()
        .copied()
        .map(|v| {
            runtime
                .js_to_string(v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?
        .unwrap_or_else(|| "undefined".into());

    let replace_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let replace_str = runtime
        .js_to_string(replace_arg)
        .map_err(|e| map_interpreter_error(e, runtime))?;

    if search.is_empty() {
        // Insert replacement between every character.
        let chars: Vec<char> = s.chars().collect();
        let mut result = String::new();
        result.push_str(&replace_str);
        for (index, ch) in chars.iter().enumerate() {
            check_interrupt_poll(runtime, index)?;
            result.push(*ch);
            result.push_str(&replace_str);
        }
        let handle = runtime.alloc_string(result);
        return Ok(RegisterValue::from_object_handle(handle.0));
    }

    let result = s.replace(&*search, &replace_str);
    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.12 String.prototype.normalize([form]) ──────────────────────────

fn string_normalize(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // Minimal: return as-is (NFC is identity for ASCII/Latin-1).
    let s = this_string_value(this, runtime)?;
    let handle = runtime.alloc_string(&*s);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Get the raw `JsString` from `this` — preserves lone surrogates.
fn this_js_string_value(
    this: &RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<crate::js_string::JsString, VmNativeCallError> {
    if let Some(handle) = this.as_object_handle().map(crate::object::ObjectHandle) {
        if let Ok(Some(js)) = runtime.objects().string_value(handle) {
            return Ok(js.clone());
        }
        // Check for String wrapper object (__otter_string_data__ slot).
        let str_prop = runtime.intern_property_name("__otter_string_data__");
        if let Ok(Some(lookup)) = runtime.property_lookup(handle, str_prop)
            && let crate::object::PropertyValue::Data { value: v, .. } = lookup.value()
            && let Some(inner_h) = v.as_object_handle().map(crate::object::ObjectHandle)
            && let Ok(Some(js)) = runtime.objects().string_value(inner_h)
        {
            return Ok(js.clone());
        }
    }
    // Fallback: coerce via js_to_string (lossy for lone surrogates)
    let s = this_string_value(this, runtime)?;
    Ok(crate::js_string::JsString::from_str(&s))
}

// ── §22.1.3.9 String.prototype.isWellFormed() ───────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-string.prototype.iswellformed>

fn string_is_well_formed(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let js = this_js_string_value(this, runtime)?;
    Ok(RegisterValue::from_bool(js.is_well_formed()))
}

// ── §22.1.3.33 String.prototype.toWellFormed() ─────────────────────────────
// Spec: <https://tc39.es/ecma262/#sec-string.prototype.towellformed>

fn string_to_well_formed(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let js = this_js_string_value(this, runtime)?;
    let result = js.to_well_formed();
    let handle = runtime.alloc_js_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.11 String.prototype.localeCompare(that [, locales [, options]]) ──
//
// ECMA-402 §18.1.1: <https://tc39.es/ecma402/#sup-string.prototype.localecompare>

fn string_locale_compare(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    use icu_collator::{Collator, CollatorPreferences};
    use icu_locale::Locale;
    use std::str::FromStr;

    let s = this_string_value(this, runtime)?;
    let that = args
        .first()
        .copied()
        .map(|v| {
            runtime
                .js_to_string(v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?
        .unwrap_or_else(|| "undefined".into());

    // Try to resolve locale from args[1] if provided.
    let locale_str = args
        .get(1)
        .filter(|v| **v != RegisterValue::undefined())
        .map(|v| {
            runtime
                .js_to_string(*v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?;

    let locale = locale_str
        .as_deref()
        .and_then(|s| Locale::from_str(s).ok())
        .unwrap_or_else(|| Locale::from_str("en-US").expect("en-US parses"));
    let prefs = CollatorPreferences::from(&locale);

    let collator = Collator::try_new(prefs, Default::default())
        .unwrap_or_else(|_| Collator::try_new(Default::default(), Default::default()).unwrap());

    let cmp = collator.compare(&s, &that);
    let result = match cmp {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(RegisterValue::from_i32(result))
}

// ── §22.1.3.22 String.prototype.toLocaleLowerCase([locale]) ──────────────
//
// ECMA-402 §18.1.2: <https://tc39.es/ecma402/#sup-string.prototype.tolocalelowercase>

fn string_to_locale_lower_case(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;

    // Determine locale for Turkic special-casing.
    let locale_str = args
        .first()
        .filter(|v| **v != RegisterValue::undefined())
        .map(|v| {
            runtime
                .js_to_string(*v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?;

    let is_turkic = locale_str
        .as_deref()
        .is_some_and(crate::intrinsics::intl::locale_utils::is_turkic_locale);

    let result = if is_turkic {
        // Turkish/Azerbaijani: I → ı, İ → i
        s.chars()
            .map(|ch| match ch {
                'I' => 'ı',
                '\u{130}' => 'i', // İ → i
                _ => ch.to_lowercase().next().unwrap_or(ch),
            })
            .collect::<String>()
    } else {
        s.to_lowercase()
    };

    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── §22.1.3.23 String.prototype.toLocaleUpperCase([locale]) ──────────────
//
// ECMA-402 §18.1.3: <https://tc39.es/ecma402/#sup-string.prototype.tolocaleuppercase>

fn string_to_locale_upper_case(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let s = this_string_value(this, runtime)?;

    let locale_str = args
        .first()
        .filter(|v| **v != RegisterValue::undefined())
        .map(|v| {
            runtime
                .js_to_string(*v)
                .map_err(|e| map_interpreter_error(e, runtime))
        })
        .transpose()?;

    let is_turkic = locale_str
        .as_deref()
        .is_some_and(crate::intrinsics::intl::locale_utils::is_turkic_locale);

    let result = if is_turkic {
        // Turkish/Azerbaijani: i → İ, ı → I
        s.chars()
            .map(|ch| match ch {
                'i' => '\u{130}', // İ
                'ı' => 'I',
                _ => ch.to_uppercase().next().unwrap_or(ch),
            })
            .collect::<String>()
    } else {
        s.to_uppercase()
    };

    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ────────────────────────────────────────────────────────────────────────────
// Static methods
// ────────────────────────────────────────────────────────────────────────────

/// `String.fromCharCode(...codeUnits)` — §22.1.2.1
/// <https://tc39.es/ecma262/#sec-string.fromcharcode>
///
/// Each argument is converted to Uint16 and interpreted as a single UTF-16
/// code unit. The resulting string is the concatenation of those code units.
fn string_from_char_code(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let mut buf: Vec<u16> = Vec::with_capacity(args.len());
    for (index, &arg) in args.iter().enumerate() {
        check_interrupt_poll(runtime, index)?;
        // §7.1.8 ToUint16 — ToNumber then modulo 2^16.
        let n = runtime
            .js_to_number(arg)
            .map_err(|e| map_interpreter_error(e, runtime))?;
        let code_unit = f64_to_uint16(n);
        buf.push(code_unit);
    }
    let result = String::from_utf16_lossy(&buf);
    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// `String.fromCodePoint(...codePoints)` — §22.1.2.2
/// <https://tc39.es/ecma262/#sec-string.fromcodepoint>
///
/// Each argument is converted to a Number, validated as a valid Unicode code
/// point (0..=0x10FFFF, integer), and then encoded into the result string.
/// Throws **RangeError** for any invalid code point.
fn string_from_code_point(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let mut result = String::with_capacity(args.len());
    for (index, &arg) in args.iter().enumerate() {
        check_interrupt_poll(runtime, index)?;
        let n = runtime
            .js_to_number(arg)
            .map_err(|e| map_interpreter_error(e, runtime))?;
        // Step 5.b: If nextCP is not an integral Number, throw a RangeError.
        // Step 5.c: If nextCP < 0 or nextCP > 0x10FFFF, throw a RangeError.
        if n.fract() != 0.0 || n < 0.0 || n > 0x10_FFFF as f64 || n.is_nan() || n.is_infinite() {
            return Err(range_error(runtime, &format!("Invalid code point {n}")));
        }
        let cp = n as u32;
        if let Some(ch) = char::from_u32(cp) {
            result.push(ch);
        } else {
            return Err(range_error(runtime, &format!("Invalid code point {cp}")));
        }
    }
    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// `String.raw(template, ...substitutions)` — §22.1.2.4
/// <https://tc39.es/ecma262/#sec-string.raw>
///
/// Reads the `.raw` property of the first argument (the template object),
/// iterates its elements, and interleaves them with the remaining arguments
/// (substitutions) after string coercion.
fn string_raw(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // Step 1-2: Let cooked = ToObject(template).
    let template = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    if template.is_undefined() || template.is_null() {
        return Err(type_error(
            runtime,
            "Cannot convert undefined or null to object",
        )?);
    }

    // Step 3: Let raw = ToObject(Get(cooked, "raw")).
    let raw_prop = runtime.intern_property_name("raw");
    let raw_val = if let Some(obj) = template.as_object_handle().map(ObjectHandle) {
        lookup_data_property(runtime, obj, raw_prop)
    } else {
        RegisterValue::undefined()
    };
    if raw_val.is_undefined() || raw_val.is_null() {
        return Err(type_error(
            runtime,
            "Cannot convert undefined or null to object",
        )?);
    }

    let raw_obj = raw_val
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("String.raw: raw is not an object".into()))?;

    // Step 4: Let literalSegments = LengthOfArrayLike(raw).
    let length_prop = runtime.intern_property_name("length");
    let length_val = lookup_data_property(runtime, raw_obj, length_prop);
    let literal_segments = runtime
        .js_to_number(length_val)
        .map_err(|e| map_interpreter_error(e, runtime))? as usize;

    // Step 5: If literalSegments == 0, return "".
    if literal_segments == 0 {
        let handle = runtime.alloc_string("");
        return Ok(RegisterValue::from_object_handle(handle.0));
    }

    let substitutions = if args.len() > 1 { &args[1..] } else { &[] };
    let mut result = String::new();

    for i in 0..literal_segments {
        check_interrupt_poll(runtime, i)?;
        // Step 7.a: Get raw[i] and ToString it.
        let segment_val = runtime
            .objects_mut()
            .get_index(raw_obj, i)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or_else(RegisterValue::undefined);
        let segment = runtime
            .js_to_string(segment_val)
            .map_err(|e| map_interpreter_error(e, runtime))?;
        result.push_str(&segment);

        // Step 7.b: If i + 1 < literalSegments, append ToString(substitutions[i]).
        if i + 1 < literal_segments
            && let Some(&sub) = substitutions.get(i)
        {
            let sub_str = runtime
                .js_to_string(sub)
                .map_err(|e| map_interpreter_error(e, runtime))?;
            result.push_str(&sub_str);
        }
    }

    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Read a named data property from an object, returning `undefined` if absent.
fn lookup_data_property(
    runtime: &crate::interpreter::RuntimeState,
    obj: ObjectHandle,
    prop: crate::property::PropertyNameId,
) -> RegisterValue {
    match runtime
        .objects()
        .get_property_with_registry(obj, prop, runtime.property_names())
    {
        Ok(Some(lookup)) => match lookup.value() {
            PropertyValue::Data { value, .. } => value,
            _ => RegisterValue::undefined(),
        },
        _ => RegisterValue::undefined(),
    }
}

/// §7.1.8 ToUint16(argument)
/// <https://tc39.es/ecma262/#sec-touint16>
fn f64_to_uint16(n: f64) -> u16 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    let int_val = n.trunc();
    let modulo = int_val % 65536.0;
    let positive = if modulo < 0.0 {
        modulo + 65536.0
    } else {
        modulo
    };
    positive as u16
}

fn number_to_string(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_string()
    } else if number.is_infinite() {
        if number.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else if number == 0.0 {
        "0".to_string()
    } else if number.fract() == 0.0 {
        format!("{number:.0}")
    } else {
        number.to_string()
    }
}

/// String.prototype\[@@iterator\]()
/// Spec: <https://tc39.es/ecma262/#sec-string.prototype-@@iterator>
fn string_iterator(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let text = runtime
        .js_to_string(*this)
        .map_err(|error| map_interpreter_error(error, runtime))?;
    let str_handle = runtime.alloc_string(text);
    let iterator = runtime.objects_mut().alloc_string_iterator(str_handle);
    // Set prototype to %StringIteratorPrototype%.
    let proto = runtime.intrinsics().string_iterator_prototype();
    runtime
        .objects_mut()
        .set_prototype(iterator, Some(proto))
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_object_handle(iterator.0))
}
