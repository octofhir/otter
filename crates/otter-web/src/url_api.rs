use std::sync::{Arc, Mutex};

use otter_vm::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use otter_vm::object::{HeapValueKind, ObjectHandle};
use otter_vm::payload::{VmTrace, VmValueTracer};
use otter_vm::{RegisterValue, RuntimeState};
use url::Url;

pub(crate) fn install(runtime: &mut RuntimeState) -> Result<(), String> {
    install_url(runtime)?;
    install_url_search_params(runtime)?;
    Ok(())
}

#[derive(Debug)]
struct UrlState {
    url: Url,
    search_params_object: Option<ObjectHandle>,
}

#[derive(Debug, Clone)]
struct UrlPayload {
    shared: Arc<Mutex<UrlState>>,
}

impl VmTrace for UrlPayload {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        if let Ok(state) = self.shared.lock() {
            state.search_params_object.trace(tracer);
        }
    }
}

#[derive(Debug, Clone)]
enum UrlSearchParamsBacking {
    Linked(Arc<Mutex<UrlState>>),
    Standalone(Arc<Mutex<Vec<(String, String)>>>),
}

#[derive(Debug, Clone)]
struct UrlSearchParamsPayload {
    backing: UrlSearchParamsBacking,
}

impl VmTrace for UrlSearchParamsPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

fn install_url(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "URL") {
        return Ok(());
    }
    let prototype = runtime.alloc_object();
    for (name, callback, arity, context) in [
        ("toString", url_to_string as _, 0, "URL.prototype.toString"),
        ("toJSON", url_to_json as _, 0, "URL.prototype.toJSON"),
    ] {
        install_method(runtime, prototype, name, arity, callback, context)?;
    }
    // W1: every settable URL property gets a paired (getter,
    // setter) — the WHATWG spec §4 declares 10 out of 12
    // properties writable (origin + searchParams stay read-only).
    // Setters re-parse the stringified new value through the
    // underlying `url` crate so `u.href = "..."` and
    // `u.hostname = "..."` both flow through IDNA / percent-
    // encoding normalisation.
    for (name, getter, setter, context) in [
        (
            "href",
            url_get_href as NativeFn,
            Some(url_set_href as NativeFn),
            "URL.prototype.href",
        ),
        (
            "protocol",
            url_get_protocol as NativeFn,
            Some(url_set_protocol as NativeFn),
            "URL.prototype.protocol",
        ),
        (
            "username",
            url_get_username as NativeFn,
            Some(url_set_username as NativeFn),
            "URL.prototype.username",
        ),
        (
            "password",
            url_get_password as NativeFn,
            Some(url_set_password as NativeFn),
            "URL.prototype.password",
        ),
        (
            "host",
            url_get_host as NativeFn,
            Some(url_set_host as NativeFn),
            "URL.prototype.host",
        ),
        (
            "hostname",
            url_get_hostname as NativeFn,
            Some(url_set_hostname as NativeFn),
            "URL.prototype.hostname",
        ),
        (
            "port",
            url_get_port as NativeFn,
            Some(url_set_port as NativeFn),
            "URL.prototype.port",
        ),
        (
            "pathname",
            url_get_pathname as NativeFn,
            Some(url_set_pathname as NativeFn),
            "URL.prototype.pathname",
        ),
        (
            "search",
            url_get_search as NativeFn,
            Some(url_set_search as NativeFn),
            "URL.prototype.search",
        ),
        (
            "hash",
            url_get_hash as NativeFn,
            Some(url_set_hash as NativeFn),
            "URL.prototype.hash",
        ),
        (
            "origin",
            url_get_origin as NativeFn,
            None,
            "URL.prototype.origin",
        ),
        (
            "searchParams",
            url_get_search_params as NativeFn,
            None,
            "URL.prototype.searchParams",
        ),
    ] {
        install_accessor(runtime, prototype, name, getter, setter, context)?;
    }

    let constructor = alloc_constructor(runtime, "URL", 1, url_constructor);
    link_constructor_and_prototype(runtime, constructor, prototype)?;
    // W1: URL.canParse — §4.3 static method. Returns true iff the
    // supplied string parses without throwing.
    install_static_method(
        runtime,
        constructor,
        "canParse",
        1,
        url_can_parse,
        "URL.canParse",
    )?;
    runtime.install_global_value("URL", RegisterValue::from_object_handle(constructor.0));
    Ok(())
}

fn install_url_search_params(runtime: &mut RuntimeState) -> Result<(), String> {
    if has_global(runtime, "URLSearchParams") {
        return Ok(());
    }
    let prototype = runtime.alloc_object();
    for (name, callback, arity, context) in [
        (
            "append",
            url_search_params_append as _,
            2,
            "URLSearchParams.prototype.append",
        ),
        (
            "delete",
            url_search_params_delete as _,
            1,
            "URLSearchParams.prototype.delete",
        ),
        (
            "get",
            url_search_params_get as _,
            1,
            "URLSearchParams.prototype.get",
        ),
        (
            "getAll",
            url_search_params_get_all as _,
            1,
            "URLSearchParams.prototype.getAll",
        ),
        (
            "has",
            url_search_params_has as _,
            1,
            "URLSearchParams.prototype.has",
        ),
        (
            "set",
            url_search_params_set as _,
            2,
            "URLSearchParams.prototype.set",
        ),
        (
            "toString",
            url_search_params_to_string as _,
            0,
            "URLSearchParams.prototype.toString",
        ),
        (
            "sort",
            url_search_params_sort as _,
            0,
            "URLSearchParams.prototype.sort",
        ),
    ] {
        install_method(runtime, prototype, name, arity, callback, context)?;
    }
    // W1: `size` is a getter, not a data property — spec §5.1.
    install_getter(
        runtime,
        prototype,
        "size",
        url_search_params_get_size,
        "URLSearchParams.prototype.size",
    )?;

    let constructor =
        alloc_constructor(runtime, "URLSearchParams", 1, url_search_params_constructor);
    link_constructor_and_prototype(runtime, constructor, prototype)?;
    runtime.install_global_value(
        "URLSearchParams",
        RegisterValue::from_object_handle(constructor.0),
    );
    Ok(())
}

fn url_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let input = string_arg(runtime, args.first(), "URL: missing input")?;
    let parsed = parse_url_arg(runtime, &input, args.get(1))?;
    let prototype = class_prototype(runtime, "URL")?;
    let instance = runtime.alloc_native_object_with_prototype(
        Some(prototype),
        UrlPayload {
            shared: Arc::new(Mutex::new(UrlState {
                url: parsed,
                search_params_object: None,
            })),
        },
    );
    Ok(RegisterValue::from_object_handle(instance.0))
}

fn url_get_href(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let href = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state.url.as_str().to_string()
    };
    Ok(string_value(runtime, href))
}

fn url_get_protocol(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let value = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        format!("{}:", state.url.scheme())
    };
    Ok(string_value(runtime, value))
}

fn url_get_username(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let value = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state.url.username().to_string()
    };
    Ok(string_value(runtime, value))
}

fn url_get_password(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let value = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state.url.password().unwrap_or_default().to_string()
    };
    Ok(string_value(runtime, value))
}

fn url_get_host(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let value = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        match state.url.port() {
            Some(port) => format!("{}:{port}", state.url.host_str().unwrap_or_default()),
            None => state.url.host_str().unwrap_or_default().to_string(),
        }
    };
    Ok(string_value(runtime, value))
}

fn url_get_hostname(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let value = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state.url.host_str().unwrap_or_default().to_string()
    };
    Ok(string_value(runtime, value))
}

fn url_get_port(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let value = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state
            .url
            .port()
            .map(|port| port.to_string())
            .unwrap_or_default()
    };
    Ok(string_value(runtime, value))
}

fn url_get_pathname(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let value = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state.url.path().to_string()
    };
    Ok(string_value(runtime, value))
}

fn url_get_search(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let value = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state
            .url
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default()
    };
    Ok(string_value(runtime, value))
}

fn url_get_hash(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let value = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state
            .url
            .fragment()
            .map(|fragment| format!("#{fragment}"))
            .unwrap_or_default()
    };
    Ok(string_value(runtime, value))
}

fn url_get_origin(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    let value = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state.url.origin().ascii_serialization()
    };
    Ok(string_value(runtime, value))
}

fn url_get_search_params(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = require_url_shared(runtime, this)?;
    if let Some(existing) = {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state.search_params_object
    } {
        return Ok(RegisterValue::from_object_handle(existing.0));
    }

    let prototype = class_prototype(runtime, "URLSearchParams")?;
    let object = runtime.alloc_native_object_with_prototype(
        Some(prototype),
        UrlSearchParamsPayload {
            backing: UrlSearchParamsBacking::Linked(shared.clone()),
        },
    );
    {
        let mut state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        state.search_params_object = Some(object);
    }
    Ok(RegisterValue::from_object_handle(object.0))
}

fn url_to_string(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    url_get_href(this, args, runtime)
}

// W1: URL setters — re-parse / mutate the underlying `Url` to
// keep percent-encoding + IDNA normalisation consistent with the
// WHATWG spec. The `url` crate exposes scheme/username/password/
// host/port/path/query/fragment mutators directly; `href` reparses
// from scratch. Setters silently discard invalid values per spec.

fn url_set_href(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_value = string_arg(runtime, args.first(), "URL.href: missing value")?;
    let shared = require_url_shared(runtime, this)?;
    let parsed =
        Url::parse(&new_value).map_err(|_| type_error(runtime, "URL: invalid href value"))?;
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
    state.url = parsed;
    Ok(RegisterValue::undefined())
}

fn url_set_protocol(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_value = string_arg(runtime, args.first(), "URL.protocol: missing value")?;
    // Per WHATWG: trailing `:` optional, leading and trailing
    // whitespace stripped, can't change a special scheme into a
    // non-special one (and vice versa). `url` crate enforces
    // most of this via `set_scheme`.
    let cleaned = new_value.trim_end_matches(':');
    let shared = require_url_shared(runtime, this)?;
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
    let _ = state.url.set_scheme(cleaned);
    Ok(RegisterValue::undefined())
}

fn url_set_username(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_value = string_arg(runtime, args.first(), "URL.username: missing value")?;
    let shared = require_url_shared(runtime, this)?;
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
    let _ = state.url.set_username(&new_value);
    Ok(RegisterValue::undefined())
}

fn url_set_password(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_value = string_arg(runtime, args.first(), "URL.password: missing value")?;
    let shared = require_url_shared(runtime, this)?;
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
    let _ = state.url.set_password(Some(&new_value));
    Ok(RegisterValue::undefined())
}

fn url_set_host(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_value = string_arg(runtime, args.first(), "URL.host: missing value")?;
    let shared = require_url_shared(runtime, this)?;
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
    // "host" includes optional `:port`; split on first ':' so we
    // can feed `set_host` + `set_port` independently.
    let (host_part, port_part) = match new_value.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (new_value.as_str(), None),
    };
    let _ = state.url.set_host(Some(host_part));
    if let Some(port) = port_part
        && let Ok(n) = port.parse::<u16>()
    {
        let _ = state.url.set_port(Some(n));
    }
    Ok(RegisterValue::undefined())
}

fn url_set_hostname(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_value = string_arg(runtime, args.first(), "URL.hostname: missing value")?;
    let shared = require_url_shared(runtime, this)?;
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
    let _ = state.url.set_host(Some(&new_value));
    Ok(RegisterValue::undefined())
}

fn url_set_port(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_value = string_arg(runtime, args.first(), "URL.port: missing value")?;
    let shared = require_url_shared(runtime, this)?;
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
    if new_value.is_empty() {
        let _ = state.url.set_port(None);
    } else if let Ok(n) = new_value.parse::<u16>() {
        let _ = state.url.set_port(Some(n));
    }
    Ok(RegisterValue::undefined())
}

fn url_set_pathname(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_value = string_arg(runtime, args.first(), "URL.pathname: missing value")?;
    let shared = require_url_shared(runtime, this)?;
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
    state.url.set_path(&new_value);
    Ok(RegisterValue::undefined())
}

fn url_set_search(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_value = string_arg(runtime, args.first(), "URL.search: missing value")?;
    let stripped = new_value.strip_prefix('?').unwrap_or(&new_value);
    let shared = require_url_shared(runtime, this)?;
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
    if stripped.is_empty() {
        state.url.set_query(None);
    } else {
        state.url.set_query(Some(stripped));
    }
    Ok(RegisterValue::undefined())
}

fn url_set_hash(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let new_value = string_arg(runtime, args.first(), "URL.hash: missing value")?;
    let stripped = new_value.strip_prefix('#').unwrap_or(&new_value);
    let shared = require_url_shared(runtime, this)?;
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
    if stripped.is_empty() {
        state.url.set_fragment(None);
    } else {
        state.url.set_fragment(Some(stripped));
    }
    Ok(RegisterValue::undefined())
}

// W1: URL.canParse(url[, base]) — §4.3 static helper. Returns
// true iff parsing would succeed; never throws.
fn url_can_parse(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(input) = args.first() else {
        return Ok(RegisterValue::from_bool(false));
    };
    let input_str = base_string(runtime, *input).unwrap_or_default();
    let ok = match args.get(1) {
        Some(base) if *base != RegisterValue::undefined() && *base != RegisterValue::null() => {
            let base_s = base_string(runtime, *base).unwrap_or_default();
            Url::parse(&base_s).and_then(|b| b.join(&input_str)).is_ok()
        }
        _ => Url::parse(&input_str).is_ok(),
    };
    Ok(RegisterValue::from_bool(ok))
}

fn url_to_json(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    url_get_href(this, args, runtime)
}

fn url_search_params_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let init = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let params = parse_search_params_init(runtime, init)?;
    let prototype = class_prototype(runtime, "URLSearchParams")?;
    let object = runtime.alloc_native_object_with_prototype(
        Some(prototype),
        UrlSearchParamsPayload {
            backing: UrlSearchParamsBacking::Standalone(Arc::new(Mutex::new(params))),
        },
    );
    Ok(RegisterValue::from_object_handle(object.0))
}

fn url_search_params_append(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = string_arg(
        runtime,
        args.first(),
        "URLSearchParams.append: missing name",
    )?;
    let value = string_arg(
        runtime,
        args.get(1),
        "URLSearchParams.append: missing value",
    )?;
    with_search_params_mut(runtime, this, |pairs| pairs.push((name, value)))?;
    Ok(RegisterValue::undefined())
}

fn url_search_params_delete(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = string_arg(
        runtime,
        args.first(),
        "URLSearchParams.delete: missing name",
    )?;
    with_search_params_mut(runtime, this, |pairs| pairs.retain(|(key, _)| key != &name))?;
    Ok(RegisterValue::undefined())
}

fn url_search_params_get(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = string_arg(runtime, args.first(), "URLSearchParams.get: missing name")?;
    let pairs = search_params_pairs(runtime, this)?;
    let value = pairs
        .into_iter()
        .find_map(|(key, value)| (key == name).then_some(value));
    Ok(match value {
        Some(value) => string_value(runtime, value),
        None => RegisterValue::null(),
    })
}

fn url_search_params_get_all(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = string_arg(
        runtime,
        args.first(),
        "URLSearchParams.getAll: missing name",
    )?;
    let pairs = search_params_pairs(runtime, this)?;
    let values: Vec<_> = pairs
        .into_iter()
        .filter_map(|(key, value)| (key == name).then_some(string_value(runtime, value)))
        .collect();
    let array = runtime.alloc_array_with_elements(&values);
    Ok(RegisterValue::from_object_handle(array.0))
}

fn url_search_params_has(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = string_arg(runtime, args.first(), "URLSearchParams.has: missing name")?;
    let pairs = search_params_pairs(runtime, this)?;
    Ok(RegisterValue::from_bool(
        pairs.into_iter().any(|(key, _)| key == name),
    ))
}

fn url_search_params_set(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let name = string_arg(runtime, args.first(), "URLSearchParams.set: missing name")?;
    let value = string_arg(runtime, args.get(1), "URLSearchParams.set: missing value")?;
    with_search_params_mut(runtime, this, |pairs| {
        let mut replaced = false;
        pairs.retain_mut(|(key, current)| {
            if key != &name {
                return true;
            }
            if !replaced {
                *current = value.clone();
                replaced = true;
                true
            } else {
                false
            }
        });
        if !replaced {
            pairs.push((name, value));
        }
    })?;
    Ok(RegisterValue::undefined())
}

fn url_search_params_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let serialized = encode_pairs(&search_params_pairs(runtime, this)?);
    Ok(string_value(runtime, serialized))
}

// W1: URLSearchParams.prototype.sort — §5.1.7. Stable sort by
// code-unit (UTF-16) comparison of the name, preserving
// relative order of pairs with the same name.
fn url_search_params_sort(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    with_search_params_mut(runtime, this, |pairs| {
        // `sort_by` is stable (since Rust 1.0), so equal-name
        // pairs keep their insertion order as the spec demands.
        pairs.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    })?;
    Ok(RegisterValue::undefined())
}

// W1: URLSearchParams.prototype.size getter — §5.1. Number of
// name/value pairs, including duplicates.
fn url_search_params_get_size(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let pairs = search_params_pairs(runtime, this)?;
    Ok(RegisterValue::from_i32(
        i32::try_from(pairs.len()).unwrap_or(i32::MAX),
    ))
}

fn require_url_shared(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<Arc<Mutex<UrlState>>, VmNativeCallError> {
    let payload = match runtime.native_payload_from_value::<UrlPayload>(value) {
        Ok(payload) => payload,
        Err(_) => {
            return Err(type_error(
                runtime,
                "URL method called on incompatible receiver",
            ));
        }
    };
    Ok(payload.shared.clone())
}

fn require_url_search_params_backing(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<UrlSearchParamsBacking, VmNativeCallError> {
    let payload = match runtime.native_payload_from_value::<UrlSearchParamsPayload>(value) {
        Ok(payload) => payload,
        Err(_) => {
            return Err(type_error(
                runtime,
                "URLSearchParams method called on incompatible receiver",
            ));
        }
    };
    Ok(payload.backing.clone())
}

fn search_params_pairs(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<Vec<(String, String)>, VmNativeCallError> {
    match require_url_search_params_backing(runtime, value)? {
        UrlSearchParamsBacking::Linked(shared) => {
            let state = shared
                .lock()
                .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
            Ok(state
                .url
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect())
        }
        UrlSearchParamsBacking::Standalone(shared) => {
            let pairs = shared.lock().map_err(|_| {
                VmNativeCallError::Internal("URLSearchParams state mutex poisoned".into())
            })?;
            Ok(pairs.clone())
        }
    }
}

fn with_search_params_mut(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
    mutate: impl FnOnce(&mut Vec<(String, String)>),
) -> Result<(), VmNativeCallError> {
    match require_url_search_params_backing(runtime, value)? {
        UrlSearchParamsBacking::Linked(shared) => {
            let mut state = shared
                .lock()
                .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
            let mut pairs: Vec<_> = state
                .url
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            mutate(&mut pairs);
            let encoded = encode_pairs(&pairs);
            if encoded.is_empty() {
                state.url.set_query(None);
            } else {
                state.url.set_query(Some(&encoded));
            }
            Ok(())
        }
        UrlSearchParamsBacking::Standalone(shared) => {
            let mut pairs = shared.lock().map_err(|_| {
                VmNativeCallError::Internal("URLSearchParams state mutex poisoned".into())
            })?;
            mutate(&mut pairs);
            Ok(())
        }
    }
}

fn parse_url_arg(
    runtime: &mut RuntimeState,
    input: &str,
    base: Option<&RegisterValue>,
) -> Result<Url, VmNativeCallError> {
    if let Some(base) = base
        && *base != RegisterValue::undefined()
        && *base != RegisterValue::null()
    {
        let base_string = base_string(runtime, *base)?;
        let base_url =
            Url::parse(&base_string).map_err(|_| type_error(runtime, "URL: invalid base URL"))?;
        return base_url
            .join(input)
            .map_err(|_| type_error(runtime, "URL: invalid URL"));
    }
    Url::parse(input).map_err(|_| type_error(runtime, "URL: invalid URL"))
}

fn base_string(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<String, VmNativeCallError> {
    if let Some(shared) = runtime
        .native_payload_from_value::<UrlPayload>(&value)
        .ok()
        .map(|payload| payload.shared.clone())
    {
        let state = shared
            .lock()
            .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
        return Ok(state.url.as_str().to_string());
    }
    Ok(runtime.js_to_string_infallible(value).into_string())
}

fn parse_search_params_init(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<Vec<(String, String)>, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(Vec::new());
    }

    if let Some(handle) = value.as_object_handle().map(ObjectHandle)
        && matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String))
    {
        return parse_search_params_string(runtime, value);
    }

    if let Some(backing) = runtime
        .native_payload_from_value::<UrlSearchParamsPayload>(&value)
        .ok()
        .map(|payload| payload.backing.clone())
    {
        return match backing {
            UrlSearchParamsBacking::Linked(shared) => {
                let state = shared
                    .lock()
                    .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
                Ok(state
                    .url
                    .query_pairs()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect())
            }
            UrlSearchParamsBacking::Standalone(shared) => {
                let pairs = shared.lock().map_err(|_| {
                    VmNativeCallError::Internal("URLSearchParams state mutex poisoned".into())
                })?;
                Ok(pairs.clone())
            }
        };
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return parse_search_params_string(runtime, value);
    };
    match runtime.objects().kind(handle) {
        Ok(HeapValueKind::Array) => {
            let values = runtime.array_to_args(handle)?;
            let mut pairs = Vec::with_capacity(values.len());
            for value in values {
                runtime.check_interrupt()?;
                let tuple = value.as_object_handle().map(ObjectHandle).ok_or_else(|| {
                    type_error(
                        runtime,
                        "URLSearchParams: expected [name, value] tuples in sequence init",
                    )
                })?;
                if runtime.objects().kind(tuple) != Ok(HeapValueKind::Array) {
                    return Err(type_error(
                        runtime,
                        "URLSearchParams: expected [name, value] tuples in sequence init",
                    ));
                }
                let parts = runtime.array_to_args(tuple)?;
                if parts.len() < 2 {
                    return Err(type_error(
                        runtime,
                        "URLSearchParams: tuple init requires [name, value]",
                    ));
                }
                pairs.push((
                    runtime.js_to_string_infallible(parts[0]).into_string(),
                    runtime.js_to_string_infallible(parts[1]).into_string(),
                ));
            }
            Ok(pairs)
        }
        _ => {
            let mut pairs = Vec::new();
            for key in runtime.enumerable_own_property_keys(handle)? {
                runtime.check_interrupt()?;
                let Some(name) = runtime.property_names().get(key).map(str::to_owned) else {
                    continue;
                };
                let value = runtime
                    .own_property_value(handle, key)
                    .unwrap_or_else(|_| RegisterValue::undefined());
                pairs.push((name, runtime.js_to_string_infallible(value).into_string()));
            }
            Ok(pairs)
        }
    }
}

fn parse_search_params_string(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<Vec<(String, String)>, VmNativeCallError> {
    let mut input = runtime.js_to_string_infallible(value).into_string();
    if let Some(stripped) = input.strip_prefix('?') {
        input = stripped.to_string();
    }
    Ok(url::form_urlencoded::parse(input.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect())
}

fn encode_pairs(pairs: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

pub(crate) fn serialize_url_search_params_value(
    runtime: &mut RuntimeState,
    value: &RegisterValue,
) -> Result<Option<String>, VmNativeCallError> {
    let Some(payload) = runtime
        .native_payload_from_value::<UrlSearchParamsPayload>(value)
        .ok()
    else {
        return Ok(None);
    };

    match &payload.backing {
        UrlSearchParamsBacking::Linked(shared) => {
            let state = shared
                .lock()
                .map_err(|_| VmNativeCallError::Internal("URL state mutex poisoned".into()))?;
            Ok(Some(encode_pairs(
                &state
                    .url
                    .query_pairs()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect::<Vec<_>>(),
            )))
        }
        UrlSearchParamsBacking::Standalone(shared) => {
            let pairs = shared.lock().map_err(|_| {
                VmNativeCallError::Internal("URLSearchParams state mutex poisoned".into())
            })?;
            Ok(Some(encode_pairs(&pairs)))
        }
    }
}

fn string_arg(
    runtime: &mut RuntimeState,
    value: Option<&RegisterValue>,
    message: &str,
) -> Result<String, VmNativeCallError> {
    let value = *value.ok_or_else(|| type_error(runtime, message))?;
    Ok(runtime.js_to_string_infallible(value).into_string())
}

fn string_value(runtime: &mut RuntimeState, value: impl Into<Box<str>>) -> RegisterValue {
    RegisterValue::from_object_handle(runtime.alloc_string(value).0)
}

fn has_global(runtime: &mut RuntimeState, name: &str) -> bool {
    let global = runtime.intrinsics().global_object();
    let property = runtime.intern_property_name(name);
    runtime
        .objects()
        .has_own_property(global, property)
        .unwrap_or(false)
}

fn class_prototype(
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

fn link_constructor_and_prototype(
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

fn alloc_constructor(
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

fn install_method(
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

fn install_getter(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    callback: NativeFn,
    context: &str,
) -> Result<(), String> {
    install_accessor(runtime, target, name, callback, None, context)
}

/// Native-function pointer signature used across this module's
/// installer helpers.
type NativeFn = fn(
    &RegisterValue,
    &[RegisterValue],
    &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError>;

/// W1: installs a paired (getter, setter) accessor on `target`.
/// `define_accessor` replaces any existing descriptor for the
/// property, so both halves must be passed in a single call.
fn install_accessor(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    getter: NativeFn,
    setter: Option<NativeFn>,
    context: &str,
) -> Result<(), String> {
    let getter_desc = NativeFunctionDescriptor::getter(name, getter);
    let getter_id = runtime.register_native_function(getter_desc);
    let getter_handle = runtime.alloc_host_function(getter_id);
    let setter_handle = setter.map(|cb| {
        let desc = NativeFunctionDescriptor::setter(name, cb);
        let id = runtime.register_native_function(desc);
        runtime.alloc_host_function(id)
    });
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .define_accessor(target, property, Some(getter_handle), setter_handle)
        .map(|_| ())
        .map_err(|error| format!("failed to install {context}: {error:?}"))
}

/// W1: installs a static method on a constructor. Mirrors
/// `install_method` but binds the function as a data property on
/// the constructor object itself (not the prototype), so
/// `URL.canParse(...)` works.
fn install_static_method(
    runtime: &mut RuntimeState,
    constructor: ObjectHandle,
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
            constructor,
            property,
            RegisterValue::from_object_handle(function.0),
        )
        .map(|_| ())
        .map_err(|error| format!("failed to install {context}: {error:?}"))
}

fn type_error(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(error) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(error.0)),
        Err(_) => VmNativeCallError::Internal(message.into()),
    }
}
