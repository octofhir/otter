//! WHATWG Blob host-side bytes.

use otter_runtime::{
    RuntimeAttr as Attr, RuntimeJsObject as JsObject, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeValue as Value, runtime_optional_arg_to_string,
    runtime_this_object, runtime_with_host_data,
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
    let text = crate::arg_string(args, 0, ctx.heap());
    let content_type = crate::arg_string(args, 1, ctx.heap());
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
