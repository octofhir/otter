//! WHATWG Blob host-side bytes.

use otter_runtime::module_api::{
    AccessorSpec, Attr, ClassSpec, ConstructorSpec, JsObject, MethodSpec, NativeCall, NativeCtx,
    NativeError, NumberValue, ObjectBuilder, Value, object,
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
pub static BLOB_CLASS_SPEC: ClassSpec = ClassSpec {
    constructor: ConstructorSpec {
        name: "Blob",
        length: 0,
        call: NativeCall::Static(blob_constructor_native),
        static_methods: &[],
        prototype_methods: &[
            method("arrayBuffer", 0, blob_array_buffer_native),
            method("slice", 2, blob_slice_native),
            method("text", 0, blob_text_native),
        ],
        attrs: Attr::global_binding(),
    },
    prototype_accessors: &[
        AccessorSpec {
            name: "size",
            get: Some(NativeCall::Static(blob_size_native)),
            set: None,
            attrs: Attr::builtin_function(),
        },
        AccessorSpec {
            name: "type",
            get: Some(NativeCall::Static(blob_type_native)),
            set: None,
            attrs: Attr::builtin_function(),
        },
    ],
};

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

fn blob_constructor_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let text = crate::arg_string(args, 0);
    let content_type = crate::arg_string(args, 1);
    blob_object(ctx, Blob::new(text.into_bytes(), content_type))
}

fn blob_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    match ctx.this_value().clone() {
        Value::Object(object) => Ok(object),
        _ => Err(crate::type_error(name, "invalid Blob receiver")),
    }
}

fn host_error(name: &'static str, err: object::HostObjectError) -> NativeError {
    crate::type_error(name, err.to_string())
}

fn blob_snapshot(ctx: &NativeCtx<'_>, name: &'static str) -> Result<Blob, NativeError> {
    let object = blob_receiver(ctx, name)?;
    object::with_host_data::<Blob, _>(object, ctx.heap(), Clone::clone)
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
    let content_type = match args.get(2) {
        Some(Value::String(value)) => Some(value.to_lossy_string()),
        Some(Value::Undefined) | None => None,
        Some(value) => Some(value.display_string()),
    };
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
    let object = object::alloc_host_object(ctx.interp_mut().gc_heap_mut(), state)?;
    let mut builder = ObjectBuilder::from_object(ctx.interp_mut().gc_heap_mut(), object);
    builder
        .property("size", size, Attr::read_only())
        .and_then(|builder| builder.property("type", content_type, Attr::read_only()))
        .and_then(|builder| {
            builder.method(
                "text",
                0,
                NativeCall::Static(blob_text_native),
                Attr::builtin_function(),
            )
        })
        .and_then(|builder| {
            builder.method(
                "arrayBuffer",
                0,
                NativeCall::Static(blob_array_buffer_native),
                Attr::builtin_function(),
            )
        })
        .and_then(|builder| {
            builder.method(
                "slice",
                2,
                NativeCall::Static(blob_slice_native),
                Attr::builtin_function(),
            )
        })
        .map_err(|err| crate::type_error("Blob", err.to_string()))?;
    Ok(Value::Object(object))
}

fn arg_usize(args: &[Value], index: usize) -> Option<usize> {
    match args.get(index) {
        Some(Value::Number(value)) if value.as_f64().is_finite() && value.as_f64() >= 0.0 => {
            Some(value.as_f64() as usize)
        }
        _ => None,
    }
}
