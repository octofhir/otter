//! WHATWG Blob host-side bytes.

use std::sync::{Arc, Mutex};

use otter_vm::{
    AccessorSpec, Attr, ClassSpec, ConstructorSpec, MethodSpec, NativeCall, NativeCtx, NativeError,
    ObjectBuilder, Value,
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
    blob_object(
        ctx,
        Arc::new(Mutex::new(Blob::new(text.into_bytes(), content_type))),
    )
}

fn blob_array_buffer_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Blob.prototype.arrayBuffer",
        "invalid Blob receiver",
    ))
}

fn blob_slice_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Blob.prototype.slice",
        "invalid Blob receiver",
    ))
}

fn blob_text_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Blob.prototype.text",
        "invalid Blob receiver",
    ))
}

fn blob_size_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Blob.prototype.size",
        "invalid Blob receiver",
    ))
}

fn blob_type_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Blob.prototype.type",
        "invalid Blob receiver",
    ))
}

pub(crate) fn blob_object(
    ctx: &mut NativeCtx<'_>,
    state: Arc<Mutex<Blob>>,
) -> Result<Value, NativeError> {
    let snapshot = state
        .lock()
        .map_err(|_| crate::type_error("Blob", "Blob state lock poisoned"))?
        .clone();
    let content_type = crate::string_value(ctx, snapshot.content_type())?;
    let size = Value::Number(otter_vm::NumberValue::from_f64(snapshot.size() as f64));
    let mut builder = ObjectBuilder::new_in_ctx(ctx)?;
    builder
        .property("size", size, Attr::read_only())
        .and_then(|builder| builder.property("type", content_type, Attr::read_only()))
        .and_then(|builder| {
            builder.method(
                "text",
                0,
                NativeCall::Dynamic(Arc::new({
                    let state = state.clone();
                    move |ctx, _args, _captures| {
                        let text = state
                            .lock()
                            .map_err(|_| {
                                crate::type_error("Blob.prototype.text", "Blob state lock poisoned")
                            })?
                            .text();
                        crate::string_value(ctx, &text)
                    }
                })),
                Attr::builtin_function(),
            )
        })
        .and_then(|builder| {
            builder.method(
                "arrayBuffer",
                0,
                NativeCall::Dynamic(Arc::new({
                    let state = state.clone();
                    move |ctx, _args, _captures| {
                        let text = state
                            .lock()
                            .map_err(|_| {
                                crate::type_error(
                                    "Blob.prototype.arrayBuffer",
                                    "Blob state lock poisoned",
                                )
                            })?
                            .text();
                        crate::string_value(ctx, &text)
                    }
                })),
                Attr::builtin_function(),
            )
        })
        .and_then(|builder| {
            builder.method(
                "slice",
                2,
                NativeCall::Dynamic(Arc::new({
                    let state = state.clone();
                    move |ctx, args, _captures| {
                        let start = arg_usize(args, 0).unwrap_or(0);
                        let end = arg_usize(args, 1);
                        let content_type = match args.get(2) {
                            Some(Value::String(value)) => Some(value.to_lossy_string()),
                            Some(Value::Undefined) | None => None,
                            Some(value) => Some(value.display_string()),
                        };
                        let blob = state
                            .lock()
                            .map_err(|_| {
                                crate::type_error(
                                    "Blob.prototype.slice",
                                    "Blob state lock poisoned",
                                )
                            })?
                            .slice(start, end, content_type.as_deref());
                        blob_object(ctx, Arc::new(Mutex::new(blob)))
                    }
                })),
                Attr::builtin_function(),
            )
        })
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
