mod headers_api;
mod url_api;

use otter_runtime::{HostedExtension, RuntimeProfile, RuntimeState};
use otter_vm::RegisterValue;
use otter_vm::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use otter_vm::object::{HeapValueKind, ObjectHandle, TypedArrayKind};
use otter_vm::payload::{VmTrace, VmValueTracer};

#[derive(Debug, Default, Clone, Copy)]
pub struct OtterWebExtension;

impl HostedExtension for OtterWebExtension {
    fn name(&self) -> &str {
        "otter-web"
    }

    fn profiles(&self) -> &[RuntimeProfile] {
        &[RuntimeProfile::Core]
    }

    fn install(&self, runtime: &mut RuntimeState) -> Result<(), String> {
        install_text_encoder(runtime)?;
        install_text_decoder(runtime)?;
        url_api::install(runtime)?;
        headers_api::install(runtime)?;
        Ok(())
    }
}

#[must_use]
pub fn web_extension() -> OtterWebExtension {
    OtterWebExtension
}

#[derive(Debug, Default)]
struct TextEncoderPayload;

impl VmTrace for TextEncoderPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

#[derive(Debug, Clone, Copy)]
struct TextDecoderPayload {
    fatal: bool,
    ignore_bom: bool,
}

impl VmTrace for TextDecoderPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

fn install_text_encoder(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "TextEncoder") {
        return Ok(());
    }
    let prototype = runtime.alloc_object();
    install_method(
        runtime,
        prototype,
        "encode",
        1,
        text_encoder_encode,
        "TextEncoder.prototype.encode",
    )?;
    install_getter(
        runtime,
        prototype,
        "encoding",
        text_encoder_get_encoding,
        "TextEncoder.prototype.encoding",
    )?;

    let constructor = alloc_constructor(runtime, "TextEncoder", 0, text_encoder_constructor);
    link_constructor_and_prototype(runtime, constructor, prototype)?;
    runtime.install_global_value(
        "TextEncoder",
        RegisterValue::from_object_handle(constructor.0),
    );
    Ok(())
}

fn install_text_decoder(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "TextDecoder") {
        return Ok(());
    }
    let prototype = runtime.alloc_object();
    install_method(
        runtime,
        prototype,
        "decode",
        1,
        text_decoder_decode,
        "TextDecoder.prototype.decode",
    )?;
    install_getter(
        runtime,
        prototype,
        "encoding",
        text_decoder_get_encoding,
        "TextDecoder.prototype.encoding",
    )?;
    install_getter(
        runtime,
        prototype,
        "fatal",
        text_decoder_get_fatal,
        "TextDecoder.prototype.fatal",
    )?;
    install_getter(
        runtime,
        prototype,
        "ignoreBOM",
        text_decoder_get_ignore_bom,
        "TextDecoder.prototype.ignoreBOM",
    )?;

    let constructor = alloc_constructor(runtime, "TextDecoder", 1, text_decoder_constructor);
    link_constructor_and_prototype(runtime, constructor, prototype)?;
    runtime.install_global_value(
        "TextDecoder",
        RegisterValue::from_object_handle(constructor.0),
    );
    Ok(())
}

fn text_encoder_constructor(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype = class_prototype(runtime, "TextEncoder")?;
    let instance = runtime.alloc_native_object_with_prototype(Some(prototype), TextEncoderPayload);
    Ok(RegisterValue::from_object_handle(instance.0))
}

fn text_encoder_encode(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_text_encoder(runtime, this)?;
    let input = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let bytes = if input == RegisterValue::undefined() {
        Vec::new()
    } else {
        runtime
            .js_to_string_infallible(input)
            .into_string()
            .into_bytes()
    };
    let value = alloc_uint8_array(runtime, bytes);
    Ok(RegisterValue::from_object_handle(value.0))
}

fn text_encoder_get_encoding(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_text_encoder(runtime, this)?;
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_string("utf-8").0,
    ))
}

fn text_decoder_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let label = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let options = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    validate_utf8_label(runtime, label)?;
    let (fatal, ignore_bom) = parse_text_decoder_options(runtime, options)?;
    let prototype = class_prototype(runtime, "TextDecoder")?;
    let instance = runtime.alloc_native_object_with_prototype(
        Some(prototype),
        TextDecoderPayload { fatal, ignore_bom },
    );
    Ok(RegisterValue::from_object_handle(instance.0))
}

fn text_decoder_decode(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_text_decoder(runtime, this)?;
    let input = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let mut bytes = bytes_from_buffer_source(runtime, input)?;
    if !payload.ignore_bom && bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.drain(..3);
    }

    let decoded = if payload.fatal {
        match String::from_utf8(bytes) {
            Ok(value) => value,
            Err(_) => return Err(type_error(runtime, "TextDecoder fatal decode failed")),
        }
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };

    Ok(RegisterValue::from_object_handle(
        runtime.alloc_string(decoded).0,
    ))
}

fn text_decoder_get_encoding(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_text_decoder(runtime, this)?;
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_string("utf-8").0,
    ))
}

fn text_decoder_get_fatal(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_text_decoder(runtime, this)?;
    Ok(RegisterValue::from_bool(payload.fatal))
}

fn text_decoder_get_ignore_bom(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let payload = require_text_decoder(runtime, this)?;
    Ok(RegisterValue::from_bool(payload.ignore_bom))
}

fn parse_text_decoder_options(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<(bool, bool), VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok((false, false));
    }
    let handle = value
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "TextDecoder options must be an object"))?;
    let fatal = own_bool_property(runtime, handle, "fatal")?.unwrap_or(false);
    let ignore_bom = own_bool_property(runtime, handle, "ignoreBOM")?.unwrap_or(false);
    Ok((fatal, ignore_bom))
}

fn validate_utf8_label(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<(), VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(());
    }
    let label = runtime.js_to_string_infallible(value).into_string();
    let normalized = label.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "" | "utf-8" | "utf8" | "unicode-1-1-utf-8"
    ) {
        Ok(())
    } else {
        Err(type_error(
            runtime,
            "TextDecoder currently supports only utf-8 labels",
        ))
    }
}

fn bytes_from_buffer_source(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<Vec<u8>, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(Vec::new());
    }
    let handle = value.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        type_error(
            runtime,
            "TextDecoder.decode expects ArrayBuffer, TypedArray, DataView, null, or undefined",
        )
    })?;

    match runtime.objects().kind(handle) {
        Ok(HeapValueKind::ArrayBuffer) => {
            let bytes = match runtime.objects().array_buffer_data(handle) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    return Err(type_error(
                        runtime,
                        "TextDecoder.decode: detached ArrayBuffer",
                    ));
                }
                Err(_) => {
                    return Err(type_error(
                        runtime,
                        "TextDecoder.decode: invalid ArrayBuffer",
                    ));
                }
            };
            Ok(bytes.to_vec())
        }
        Ok(HeapValueKind::TypedArray) => {
            let viewed_buffer = runtime
                .objects()
                .typed_array_viewed_buffer(handle)
                .map_err(|_| type_error(runtime, "TextDecoder.decode: invalid TypedArray"))?;
            let byte_offset = runtime
                .objects()
                .typed_array_byte_offset(handle)
                .map_err(|_| {
                    type_error(runtime, "TextDecoder.decode: invalid TypedArray offset")
                })?;
            let byte_length = runtime
                .objects()
                .typed_array_byte_length(handle)
                .map_err(|_| {
                    type_error(runtime, "TextDecoder.decode: invalid TypedArray length")
                })?;
            let bytes = match runtime.objects().array_buffer_data(viewed_buffer) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    return Err(type_error(
                        runtime,
                        "TextDecoder.decode: detached TypedArray buffer",
                    ));
                }
                Err(_) => {
                    return Err(type_error(
                        runtime,
                        "TextDecoder.decode: invalid TypedArray buffer",
                    ));
                }
            };
            Ok(bytes[byte_offset..byte_offset + byte_length].to_vec())
        }
        Ok(HeapValueKind::DataView) => {
            let viewed_buffer = runtime
                .objects()
                .data_view_viewed_buffer(handle)
                .map_err(|_| type_error(runtime, "TextDecoder.decode: invalid DataView"))?;
            let byte_offset = runtime
                .objects()
                .data_view_byte_offset(handle)
                .map_err(|_| type_error(runtime, "TextDecoder.decode: invalid DataView offset"))?;
            let byte_length = runtime
                .objects()
                .data_view_byte_length(handle)
                .map_err(|_| type_error(runtime, "TextDecoder.decode: invalid DataView length"))?;
            let bytes = match runtime.objects().array_buffer_data(viewed_buffer) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    return Err(type_error(
                        runtime,
                        "TextDecoder.decode: detached DataView buffer",
                    ));
                }
                Err(_) => {
                    return Err(type_error(
                        runtime,
                        "TextDecoder.decode: invalid DataView buffer",
                    ));
                }
            };
            Ok(bytes[byte_offset..byte_offset + byte_length].to_vec())
        }
        _ => Err(type_error(
            runtime,
            "TextDecoder.decode expects ArrayBuffer, TypedArray, DataView, null, or undefined",
        )),
    }
}

fn alloc_uint8_array(runtime: &mut RuntimeState, bytes: Vec<u8>) -> ObjectHandle {
    let buffer_proto = Some(runtime.intrinsics().array_buffer_prototype());
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(bytes.clone(), buffer_proto);
    let (_, uint8_proto) = runtime
        .intrinsics()
        .typed_array_constructor_prototype(TypedArrayKind::Uint8);
    runtime.objects_mut().alloc_typed_array(
        TypedArrayKind::Uint8,
        buffer,
        0,
        bytes.len(),
        Some(uint8_proto),
    )
}

pub(crate) fn class_prototype(
    runtime: &mut RuntimeState,
    global_name: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    let global = runtime.intrinsics().global_object();
    let ctor_prop = runtime.intern_property_name(global_name);
    let ctor = runtime.own_property_value(global, ctor_prop).map_err(|_| {
        type_error(
            runtime,
            &format!("{global_name} constructor is not installed"),
        )
    })?;
    let ctor = ctor
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, &format!("{global_name} constructor is invalid")))?;
    let proto_prop = runtime.intern_property_name("prototype");
    runtime
        .own_property_value(ctor, proto_prop)
        .map_err(|_| type_error(runtime, &format!("{global_name}.prototype is unavailable")))?
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, &format!("{global_name}.prototype is invalid")))
}

fn require_text_encoder<'a>(
    runtime: &'a mut RuntimeState,
    value: &RegisterValue,
) -> Result<&'a TextEncoderPayload, VmNativeCallError> {
    if runtime
        .native_payload_from_value::<TextEncoderPayload>(value)
        .is_err()
    {
        return Err(type_error(
            runtime,
            "TextEncoder method called on incompatible receiver",
        ));
    }
    runtime
        .native_payload_from_value::<TextEncoderPayload>(value)
        .map_err(|_| VmNativeCallError::Internal("TextEncoder payload lookup failed".into()))
}

fn require_text_decoder(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<TextDecoderPayload, VmNativeCallError> {
    runtime
        .native_payload_from_value::<TextDecoderPayload>(value)
        .copied()
        .map_err(|_| {
            type_error(
                runtime,
                "TextDecoder method called on incompatible receiver",
            )
        })
}

pub(crate) fn has_global(runtime: &mut RuntimeState, name: &str) -> bool {
    let global = runtime.intrinsics().global_object();
    let property = runtime.intern_property_name(name);
    runtime
        .objects()
        .has_own_property(global, property)
        .unwrap_or(false)
}

fn own_bool_property(
    runtime: &mut RuntimeState,
    object: ObjectHandle,
    name: &str,
) -> Result<Option<bool>, VmNativeCallError> {
    let property = runtime.intern_property_name(name);
    let value = runtime
        .own_property_value(object, property)
        .unwrap_or_else(|_| RegisterValue::undefined());
    if value == RegisterValue::undefined() {
        Ok(None)
    } else if let Some(boolean) = value.as_bool() {
        Ok(Some(boolean))
    } else if let Some(number) = value.as_number() {
        Ok(Some(number != 0.0))
    } else {
        Ok(Some(true))
    }
}

pub(crate) fn link_constructor_and_prototype(
    runtime: &mut RuntimeState,
    constructor: ObjectHandle,
    prototype: ObjectHandle,
) -> Result<(), String> {
    let prototype_property = runtime.intern_property_name("prototype");
    runtime
        .objects_mut()
        .set_property(
            constructor,
            prototype_property,
            RegisterValue::from_object_handle(prototype.0),
        )
        .map_err(|error| format!("failed to install class prototype: {error:?}"))?;
    let constructor_property = runtime.intern_property_name("constructor");
    runtime
        .objects_mut()
        .set_property(
            prototype,
            constructor_property,
            RegisterValue::from_object_handle(constructor.0),
        )
        .map_err(|error| format!("failed to install class constructor backlink: {error:?}"))?;
    Ok(())
}

pub(crate) fn alloc_constructor(
    runtime: &mut RuntimeState,
    name: &str,
    arity: u16,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> ObjectHandle {
    let descriptor = NativeFunctionDescriptor::constructor(name, arity, callback);
    let id = runtime.register_native_function(descriptor);
    runtime.alloc_host_function(id)
}

pub(crate) fn install_method(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    arity: u16,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
    context: &str,
) -> Result<(), String> {
    let descriptor = NativeFunctionDescriptor::method(name, arity, callback);
    let id = runtime.register_native_function(descriptor);
    let function = runtime.alloc_host_function(id);
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .set_property(
            target,
            property,
            RegisterValue::from_object_handle(function.0),
        )
        .map(|_| ())
        .map_err(|error| format!("failed to install {context}: {error:?}"))
}

pub(crate) fn install_getter(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
    context: &str,
) -> Result<(), String> {
    let descriptor = NativeFunctionDescriptor::getter(name, callback);
    let id = runtime.register_native_function(descriptor);
    let getter = runtime.alloc_host_function(id);
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .define_accessor(target, property, Some(getter), None)
        .map(|_| ())
        .map_err(|error| format!("failed to install {context}: {error:?}"))
}

pub(crate) fn type_error(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(error) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(error.0)),
        Err(_) => VmNativeCallError::Internal(message.into()),
    }
}
