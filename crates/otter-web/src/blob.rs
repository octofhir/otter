//! WHATWG Blob host-side bytes.

use otter_runtime::{
    RuntimeAttr as Attr, RuntimeClassSpec as ClassSpec, RuntimeJsObject as JsObject,
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeNumberValue as NumberValue, RuntimeObjectBuilder as ObjectBuilder,
    RuntimeValue as Value, runtime_class, runtime_constructor, runtime_getter, runtime_method,
    runtime_optional_arg_to_string, runtime_this_object, runtime_with_host_data,
};

/// Owned Blob data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    bytes: Vec<u8>,
    content_type: String,
}

impl Blob {
    /// Create a Blob from owned bytes and a MIME type.
    #[must_use]
    pub fn new(bytes: Vec<u8>, content_type: impl Into<String>) -> Self {
        Self {
            bytes,
            content_type: normalize_type(&content_type.into()),
        }
    }

    /// Byte length.
    #[must_use]
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// MIME type.
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Copy bytes out.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Slice bytes using Web Blob semantics.
    #[must_use]
    pub fn slice(&self, start: usize, end: Option<usize>, content_type: Option<&str>) -> Self {
        let end = end.unwrap_or(self.bytes.len()).min(self.bytes.len());
        let start = start.min(end);
        Self::new(
            self.bytes[start..end].to_vec(),
            content_type.unwrap_or(&self.content_type),
        )
    }

    /// Interpret bytes as UTF-8 with replacement.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

fn normalize_type(value: &str) -> String {
    if value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        value.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// Static Blob class spec.
static BLOB_PROTOTYPE_METHODS: &[otter_runtime::RuntimeMethodSpec] = &[
    runtime_method("arrayBuffer", 0, blob_array_buffer_native),
    runtime_method("slice", 2, blob_slice_native),
    runtime_method("text", 0, blob_text_native),
];

static BLOB_PROTOTYPE_ACCESSORS: &[otter_runtime::RuntimeAccessorSpec] = &[
    runtime_getter("size", blob_size_native),
    runtime_getter("type", blob_type_native),
];

pub static BLOB_CLASS_SPEC: ClassSpec = runtime_class(
    runtime_constructor(
        "Blob",
        0,
        blob_constructor_native,
        &[],
        BLOB_PROTOTYPE_METHODS,
        Attr::global_binding(),
    ),
    BLOB_PROTOTYPE_ACCESSORS,
);

fn blob_constructor_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let text = crate::arg_string(args, 0);
    let content_type = crate::arg_string(args, 1);
    blob_object(ctx, Blob::new(text.into_bytes(), content_type))
}

fn blob_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    runtime_this_object(ctx, name, "Blob")
}

fn host_error(name: &'static str, err: otter_runtime::RuntimeHostObjectError) -> NativeError {
    crate::type_error(name, err.to_string())
}

fn blob_snapshot(ctx: &NativeCtx<'_>, name: &'static str) -> Result<Blob, NativeError> {
    let object = blob_receiver(ctx, name)?;
    runtime_with_host_data::<Blob, _>(ctx, object, Clone::clone)
        .map_err(|err| host_error(name, err))
}

fn blob_array_buffer_native(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let text = blob_snapshot(ctx, "Blob.prototype.arrayBuffer")?.text();
    crate::string_value(ctx, &text)
}

fn blob_slice_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let blob = blob_snapshot(ctx, "Blob.prototype.slice")?;
    let start = arg_usize(args, 0).unwrap_or(0);
    let end = arg_usize(args, 1);
    let content_type = runtime_optional_arg_to_string(args, 2);
    blob_object(ctx, blob.slice(start, end, content_type.as_deref()))
}

fn blob_text_native(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let text = blob_snapshot(ctx, "Blob.prototype.text")?.text();
    crate::string_value(ctx, &text)
}

fn blob_size_native(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let size = blob_snapshot(ctx, "Blob.prototype.size")?.size();
    Ok(Value::Number(NumberValue::from_f64(size as f64)))
}

fn blob_type_native(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let content_type = blob_snapshot(ctx, "Blob.prototype.type")?
        .content_type()
        .to_string();
    crate::string_value(ctx, &content_type)
}

pub(crate) fn blob_object(ctx: &mut NativeCtx<'_>, state: Blob) -> Result<Value, NativeError> {
    let content_type = crate::string_value(ctx, state.content_type())?;
    let size = Value::Number(NumberValue::from_f64(state.size() as f64));
    let mut builder = ObjectBuilder::from_host_data(ctx, state)?;
    builder
        .readonly_property("size", size)
        .and_then(|builder| builder.readonly_property("type", content_type))
        .and_then(|builder| builder.builtin_method("text", 0, blob_text_native))
        .and_then(|builder| builder.builtin_method("arrayBuffer", 0, blob_array_buffer_native))
        .and_then(|builder| builder.builtin_method("slice", 2, blob_slice_native))
        .map_err(|err| crate::type_error("Blob", err.to_string()))?;
    Ok(Value::Object(builder.build()))
}

fn arg_usize(args: &[Value], index: usize) -> Option<usize> {
    match args.get(index) {
        Some(Value::Number(value)) if value.as_f64().is_finite() && value.as_f64() >= 0.0 => {
            Some(value.as_f64() as usize)
        }
        _ => None,
    }
}
