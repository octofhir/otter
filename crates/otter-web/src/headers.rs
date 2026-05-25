//! WHATWG Headers host-side list.

use std::collections::BTreeMap;

use otter_runtime::{
    RuntimeHostObjectError, RuntimeJsObject as JsObject, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeObjectBuilder as ObjectBuilder,
    RuntimeValue as Value, runtime_this_object, runtime_with_host_data, runtime_with_host_data_mut,
};

/// Headers validation error.
#[derive(Debug, thiserror::Error)]
pub enum HeadersError {
    /// Invalid header name.
    #[error("invalid header name `{0}`")]
    InvalidName(String),
}

/// Result alias for Headers operations.
pub type HeadersResult<T> = Result<T, HeadersError>;

/// Ordered, normalized header list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    entries: BTreeMap<String, Vec<String>>,
}

impl Headers {
    /// Create an empty header list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a value.
    pub fn append(&mut self, name: &str, value: &str) -> HeadersResult<()> {
        let name = normalize_name(name)?;
        self.entries
            .entry(name)
            .or_default()
            .push(normalize_value(value));
        Ok(())
    }

    /// Set a value, replacing previous values.
    pub fn set(&mut self, name: &str, value: &str) -> HeadersResult<()> {
        let name = normalize_name(name)?;
        self.entries.insert(name, vec![normalize_value(value)]);
        Ok(())
    }

    /// Delete a header.
    pub fn delete(&mut self, name: &str) -> HeadersResult<()> {
        let name = normalize_name(name)?;
        self.entries.remove(&name);
        Ok(())
    }

    /// Combined header value.
    pub fn get(&self, name: &str) -> HeadersResult<Option<String>> {
        let name = normalize_name(name)?;
        Ok(self.entries.get(&name).map(|values| values.join(", ")))
    }

    /// Whether the header exists.
    pub fn has(&self, name: &str) -> HeadersResult<bool> {
        let name = normalize_name(name)?;
        Ok(self.entries.contains_key(&name))
    }

    /// Deterministic header entries.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|(name, values)| (name.clone(), values.join(", ")))
            .collect()
    }
}

fn normalize_name(name: &str) -> HeadersResult<String> {
    if name.is_empty()
        || !name.bytes().all(|byte| {
            matches!(
                byte,
                b'!' | b'#'..=b'\'' | b'*' | b'+' | b'-' | b'.' | b'0'..=b'9'
                    | b'A'..=b'Z' | b'^' | b'_' | b'`' | b'a'..=b'z' | b'|'
                    | b'~'
            )
        })
    {
        return Err(HeadersError::InvalidName(name.to_string()));
    }
    Ok(name.to_ascii_lowercase())
}

fn normalize_value(value: &str) -> String {
    value.trim_matches(|c| c == ' ' || c == '\t').to_string()
}

otter_macros::couch! {
    name = "Headers",
    feature = WEB,
    constructor = (length = 0, call = headers_constructor_native),
    prototype = {
        methods = {
            "append"  / 2 => headers_append_native,
            "delete"  / 1 => headers_delete_native,
            "get"     / 1 => headers_get_native,
            "has"     / 1 => headers_has_native,
            "set"     / 2 => headers_set_native,
            "entries" / 0 => headers_entries_native,
        },
    },
}

fn headers_constructor_native(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    headers_object(ctx, Headers::new())
}

fn headers_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    runtime_this_object(ctx, name, "Headers")
}

fn host_error(name: &'static str, err: RuntimeHostObjectError) -> NativeError {
    crate::type_error(name, err.to_string())
}

fn headers_append_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = headers_receiver(ctx, "Headers.prototype.append")?;
    let name = crate::arg_string(args, 0, ctx.heap());
    let value = crate::arg_string(args, 1, ctx.heap());
    let result = runtime_with_host_data_mut::<Headers, _>(ctx, object, |headers| {
        headers.append(&name, &value)
    })
    .map_err(|err| host_error("Headers.prototype.append", err))?;
    result.map_err(|err| crate::type_error("Headers.prototype.append", err.to_string()))?;
    Ok(Value::undefined())
}

fn headers_delete_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = headers_receiver(ctx, "Headers.prototype.delete")?;
    let name = crate::arg_string(args, 0, ctx.heap());
    let result =
        runtime_with_host_data_mut::<Headers, _>(ctx, object, |headers| headers.delete(&name))
            .map_err(|err| host_error("Headers.prototype.delete", err))?;
    result.map_err(|err| crate::type_error("Headers.prototype.delete", err.to_string()))?;
    Ok(Value::undefined())
}

fn headers_get_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = headers_receiver(ctx, "Headers.prototype.get")?;
    let name = crate::arg_string(args, 0, ctx.heap());
    let result = runtime_with_host_data::<Headers, _>(ctx, object, |headers| headers.get(&name))
        .map_err(|err| host_error("Headers.prototype.get", err))?;
    match result.map_err(|err| crate::type_error("Headers.prototype.get", err.to_string()))? {
        Some(value) => crate::string_value(ctx, &value),
        None => Ok(Value::null()),
    }
}

fn headers_has_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = headers_receiver(ctx, "Headers.prototype.has")?;
    let name = crate::arg_string(args, 0, ctx.heap());
    let result = runtime_with_host_data::<Headers, _>(ctx, object, |headers| headers.has(&name))
        .map_err(|err| host_error("Headers.prototype.has", err))?;
    Ok(Value::boolean(result.map_err(|err| {
        crate::type_error("Headers.prototype.has", err.to_string())
    })?))
}

fn headers_set_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let object = headers_receiver(ctx, "Headers.prototype.set")?;
    let name = crate::arg_string(args, 0, ctx.heap());
    let value = crate::arg_string(args, 1, ctx.heap());
    let result =
        runtime_with_host_data_mut::<Headers, _>(ctx, object, |headers| headers.set(&name, &value))
            .map_err(|err| host_error("Headers.prototype.set", err))?;
    result.map_err(|err| crate::type_error("Headers.prototype.set", err.to_string()))?;
    Ok(Value::undefined())
}

fn headers_entries_native(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let object = headers_receiver(ctx, "Headers.prototype.entries")?;
    let entries = runtime_with_host_data::<Headers, _>(ctx, object, Headers::entries)
        .map_err(|err| host_error("Headers.prototype.entries", err))?;
    let text = entries
        .into_iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("\n");
    crate::string_value(ctx, &text)
}

pub(crate) fn headers_object(
    ctx: &mut NativeCtx<'_>,
    state: Headers,
) -> Result<Value, NativeError> {
    let mut builder = ObjectBuilder::from_host_data(ctx, state)?;
    builder
        .builtin_method("append", 2, headers_append_native)
        .and_then(|builder| builder.builtin_method("delete", 1, headers_delete_native))
        .and_then(|builder| builder.builtin_method("get", 1, headers_get_native))
        .and_then(|builder| builder.builtin_method("has", 1, headers_has_native))
        .and_then(|builder| builder.builtin_method("set", 2, headers_set_native))
        .and_then(|builder| builder.builtin_method("entries", 0, headers_entries_native))
        .map_err(|err| crate::type_error("Headers", err.to_string()))?;
    Ok(Value::object(builder.build()))
}
