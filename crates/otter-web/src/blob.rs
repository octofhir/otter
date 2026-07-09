//! WHATWG Blob host-side bytes.

use otter_runtime::{
    RuntimeAttr as Attr, RuntimeJsObject as JsObject, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeValue as Value, array, object, runtime_arg_to_string,
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

otter_macros::couch! {
    name = "Blob",
    feature = WEB,
    constructor = (length = 0, call = blob_constructor_native),
    prototype = {
        methods = {
            "arrayBuffer" / 0 => blob_array_buffer_native,
            "slice"       / 2 => blob_slice_native,
            "text"        / 0 => blob_text_native,
        },
        accessors = [
            ("size", get = blob_size_native),
            ("type", get = blob_type_native),
        ],
    },
}

fn blob_constructor_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = args.first().copied().unwrap_or_else(Value::undefined);
    let options = args.get(1).copied().unwrap_or_else(Value::undefined);
    let bytes = assemble_blob_parts(ctx, parts)?;
    let content_type = blob_options_type(ctx, options);
    blob_object(ctx, Blob::new(bytes, content_type))
}

/// Read the `type` member of the Blob/File options bag (a `USVString`). Absent,
/// non-object, or `undefined` values yield an empty type, which `Blob::new`
/// normalizes.
fn blob_options_type(ctx: &NativeCtx<'_>, options: Value) -> String {
    let Some(obj) = options.as_object() else {
        return String::new();
    };
    match object::get(obj, ctx.heap(), "type") {
        Some(value) if !value.is_undefined() => runtime_arg_to_string(&[value], 0, ctx.heap()),
        _ => String::new(),
    }
}

/// Assemble the concatenated bytes of a `sequence<BlobPart>`. Each part is a
/// string (UTF-8 encoded), a `BufferSource` (ArrayBuffer or typed-array view,
/// copied byte-for-byte), or another `Blob` (its bytes). A non-array argument is
/// treated as a single part; `undefined` yields an empty Blob.
fn assemble_blob_parts(ctx: &NativeCtx<'_>, parts: Value) -> Result<Vec<u8>, NativeError> {
    let mut out = Vec::new();
    if let Some(array) = parts.as_array() {
        let elements: Vec<Value> = array::with_elements(array, ctx.heap(), <[Value]>::to_vec);
        for element in elements {
            append_blob_part(ctx, element, &mut out)?;
        }
    } else if !parts.is_undefined() {
        append_blob_part(ctx, parts, &mut out)?;
    }
    Ok(out)
}

/// Append one BlobPart's bytes to `out`, dispatching on its runtime shape.
fn append_blob_part(
    ctx: &NativeCtx<'_>,
    value: Value,
    out: &mut Vec<u8>,
) -> Result<(), NativeError> {
    // A nested Blob (or File) contributes its raw bytes.
    if let Some(object) = value.as_object()
        && let Ok(mut bytes) =
            runtime_with_host_data::<Blob, _>(ctx, object, |blob| blob.bytes().to_vec())
    {
        out.append(&mut bytes);
        return Ok(());
    }
    // A typed-array view copies its live window.
    if let Some(view) = value.as_typed_array(ctx.heap()) {
        let offset = view.byte_offset(ctx.heap());
        let length = view.byte_length(ctx.heap());
        let bytes = view
            .buffer(ctx.heap())
            .with_bytes(ctx.heap(), |bytes| {
                bytes.get(offset..offset + length).map(<[u8]>::to_vec)
            })
            .unwrap_or_default();
        out.extend_from_slice(&bytes);
        return Ok(());
    }
    // A bare ArrayBuffer copies all of its bytes.
    if let Some(buffer) = value.as_array_buffer() {
        let bytes = buffer.with_bytes(ctx.heap(), <[u8]>::to_vec);
        out.extend_from_slice(&bytes);
        return Ok(());
    }
    // Anything else is coerced to a string and UTF-8 encoded.
    out.extend_from_slice(runtime_arg_to_string(&[value], 0, ctx.heap()).as_bytes());
    Ok(())
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
    const NAME: &str = "Blob.prototype.arrayBuffer";
    let bytes = blob_snapshot(ctx, NAME)?.bytes().to_vec();
    let buffer = ctx
        .array_buffer_from_bytes_rooted(bytes, &[], &[])
        .map_err(|err| crate::type_error(NAME, err.to_string()))?;
    let value = Value::array_buffer(buffer);
    let promise = ctx
        .fulfilled_promise_with_roots(value, &[&value], &[])
        .map_err(|err| crate::type_error(NAME, err.to_string()))?;
    Ok(Value::promise(promise))
}

fn blob_slice_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let blob = blob_snapshot(ctx, "Blob.prototype.slice")?;
    let start = arg_usize(args, 0).unwrap_or(0);
    let end = arg_usize(args, 1);
    let content_type = runtime_optional_arg_to_string(args, 2, ctx.heap());
    blob_object(ctx, blob.slice(start, end, content_type.as_deref()))
}

fn blob_text_native(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let text = blob_snapshot(ctx, "Blob.prototype.text")?.text();
    crate::string_value(ctx, &text)
}

fn blob_size_native(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let size = blob_snapshot(ctx, "Blob.prototype.size")?.size();
    Ok(Value::number_f64(size as f64))
}

fn blob_type_native(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let content_type = blob_snapshot(ctx, "Blob.prototype.type")?
        .content_type()
        .to_string();
    crate::string_value(ctx, &content_type)
}

pub(crate) fn blob_object(ctx: &mut NativeCtx<'_>, state: Blob) -> Result<Value, NativeError> {
    // Snapshot the fields as Rust values before moving `state` into the host
    // object; each JS value is minted inside the scope right before its define.
    let size = state.size() as f64;
    let content_type = state.content_type().to_string();
    ctx.scope(|ctx, s| {
        let obj = ctx.scoped_host_object(s, state)?;
        let read_only = Attr::read_only().to_flags();
        let size_value = ctx.scoped_number(s, size);
        ctx.scoped_define_data(s, obj, "size", size_value, read_only)?;
        let type_value = ctx.scoped_string(s, &content_type)?;
        ctx.scoped_define_data(s, obj, "type", type_value, read_only)?;
        let builtin = Attr::builtin_function().to_flags();
        let text = ctx.scoped_native_method(s, "text", 0, blob_text_native)?;
        ctx.scoped_define_data(s, obj, "text", text, builtin)?;
        let array_buffer =
            ctx.scoped_native_method(s, "arrayBuffer", 0, blob_array_buffer_native)?;
        ctx.scoped_define_data(s, obj, "arrayBuffer", array_buffer, builtin)?;
        let slice = ctx.scoped_native_method(s, "slice", 2, blob_slice_native)?;
        ctx.scoped_define_data(s, obj, "slice", slice, builtin)?;
        Ok::<Value, NativeError>(ctx.escape(obj))
    })
}

fn arg_usize(args: &[Value], index: usize) -> Option<usize> {
    let n = args.get(index)?.as_number()?;
    let f = n.as_f64();
    if f.is_finite() && f >= 0.0 {
        Some(f as usize)
    } else {
        None
    }
}
