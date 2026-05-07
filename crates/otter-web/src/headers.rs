//! WHATWG Headers host-side list.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use otter_vm::{
    Attr, ClassSpec, ConstructorSpec, MethodSpec, NativeCall, NativeCtx, NativeError,
    ObjectBuilder, Value,
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

/// Static Headers class spec.
pub static HEADERS_CLASS_SPEC: ClassSpec = ClassSpec {
    constructor: ConstructorSpec {
        name: "Headers",
        length: 0,
        call: NativeCall::Static(headers_constructor_native),
        static_methods: &[],
        prototype_methods: &[
            method("append", 2, headers_append_native),
            method("delete", 1, headers_delete_native),
            method("get", 1, headers_get_native),
            method("has", 1, headers_has_native),
            method("set", 2, headers_set_native),
            method("entries", 0, headers_entries_native),
        ],
        attrs: Attr::global_binding(),
    },
    prototype_accessors: &[],
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

fn headers_constructor_native(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    headers_object(ctx, Arc::new(Mutex::new(Headers::new())))
}

fn headers_append_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Headers.prototype.append",
        "invalid Headers receiver",
    ))
}

fn headers_delete_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Headers.prototype.delete",
        "invalid Headers receiver",
    ))
}

fn headers_get_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Headers.prototype.get",
        "invalid Headers receiver",
    ))
}

fn headers_has_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Headers.prototype.has",
        "invalid Headers receiver",
    ))
}

fn headers_set_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Headers.prototype.set",
        "invalid Headers receiver",
    ))
}

fn headers_entries_native(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
    Err(crate::type_error(
        "Headers.prototype.entries",
        "invalid Headers receiver",
    ))
}

pub(crate) fn headers_object(
    ctx: &mut NativeCtx<'_>,
    state: Arc<Mutex<Headers>>,
) -> Result<Value, NativeError> {
    let object = {
        let mut builder = ObjectBuilder::new_in_ctx(ctx)?;
        builder
            .method(
                "append",
                2,
                NativeCall::Dynamic(Arc::new({
                    let state = state.clone();
                    move |_ctx, args, _captures| {
                        let name = crate::arg_string(args, 0);
                        let value = crate::arg_string(args, 1);
                        state
                            .lock()
                            .map_err(|_| {
                                crate::type_error(
                                    "Headers.prototype.append",
                                    "Headers state lock poisoned",
                                )
                            })?
                            .append(&name, &value)
                            .map_err(|err| {
                                crate::type_error("Headers.prototype.append", err.to_string())
                            })?;
                        Ok(Value::Undefined)
                    }
                })),
                Attr::builtin_function(),
            )
            .and_then(|builder| {
                builder.method(
                    "delete",
                    1,
                    NativeCall::Dynamic(Arc::new({
                        let state = state.clone();
                        move |_ctx, args, _captures| {
                            let name = crate::arg_string(args, 0);
                            state
                                .lock()
                                .map_err(|_| {
                                    crate::type_error(
                                        "Headers.prototype.delete",
                                        "Headers state lock poisoned",
                                    )
                                })?
                                .delete(&name)
                                .map_err(|err| {
                                    crate::type_error("Headers.prototype.delete", err.to_string())
                                })?;
                            Ok(Value::Undefined)
                        }
                    })),
                    Attr::builtin_function(),
                )
            })
            .and_then(|builder| {
                builder.method(
                    "get",
                    1,
                    NativeCall::Dynamic(Arc::new({
                        let state = state.clone();
                        move |ctx, args, _captures| {
                            let name = crate::arg_string(args, 0);
                            let value = state
                                .lock()
                                .map_err(|_| {
                                    crate::type_error(
                                        "Headers.prototype.get",
                                        "Headers state lock poisoned",
                                    )
                                })?
                                .get(&name)
                                .map_err(|err| {
                                    crate::type_error("Headers.prototype.get", err.to_string())
                                })?;
                            match value {
                                Some(value) => crate::string_value(ctx, &value),
                                None => Ok(Value::Null),
                            }
                        }
                    })),
                    Attr::builtin_function(),
                )
            })
            .and_then(|builder| {
                builder.method(
                    "has",
                    1,
                    NativeCall::Dynamic(Arc::new({
                        let state = state.clone();
                        move |_ctx, args, _captures| {
                            let name = crate::arg_string(args, 0);
                            let has = state
                                .lock()
                                .map_err(|_| {
                                    crate::type_error(
                                        "Headers.prototype.has",
                                        "Headers state lock poisoned",
                                    )
                                })?
                                .has(&name)
                                .map_err(|err| {
                                    crate::type_error("Headers.prototype.has", err.to_string())
                                })?;
                            Ok(Value::Boolean(has))
                        }
                    })),
                    Attr::builtin_function(),
                )
            })
            .and_then(|builder| {
                builder.method(
                    "set",
                    2,
                    NativeCall::Dynamic(Arc::new({
                        let state = state.clone();
                        move |_ctx, args, _captures| {
                            let name = crate::arg_string(args, 0);
                            let value = crate::arg_string(args, 1);
                            state
                                .lock()
                                .map_err(|_| {
                                    crate::type_error(
                                        "Headers.prototype.set",
                                        "Headers state lock poisoned",
                                    )
                                })?
                                .set(&name, &value)
                                .map_err(|err| {
                                    crate::type_error("Headers.prototype.set", err.to_string())
                                })?;
                            Ok(Value::Undefined)
                        }
                    })),
                    Attr::builtin_function(),
                )
            })
            .and_then(|builder| {
                builder.method(
                    "entries",
                    0,
                    NativeCall::Dynamic(Arc::new({
                        let state = state.clone();
                        move |ctx, _args, _captures| {
                            let entries = state
                                .lock()
                                .map_err(|_| {
                                    crate::type_error(
                                        "Headers.prototype.entries",
                                        "Headers state lock poisoned",
                                    )
                                })?
                                .entries();
                            let text = entries
                                .into_iter()
                                .map(|(name, value)| format!("{name}: {value}"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            crate::string_value(ctx, &text)
                        }
                    })),
                    Attr::builtin_function(),
                )
            })
            .map_err(|err| crate::type_error("Headers", err.to_string()))?;
        builder.build()
    };
    Ok(Value::Object(object))
}
