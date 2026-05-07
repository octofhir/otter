//! WHATWG URL host-side record.

use otter_runtime::module_api::{
    Attr, ClassSpec, ConstructorSpec, JsObject, MethodSpec, NativeCall, NativeCtx, NativeError,
    ObjectBuilder, Value, object,
};
use url::Url;

/// Errors produced by URL parsing/mutation.
#[derive(Debug, thiserror::Error)]
pub enum UrlError {
    /// URL parser rejected input.
    #[error("invalid URL: {0}")]
    Invalid(String),
}

/// Result alias for URL operations.
pub type UrlResult<T> = Result<T, UrlError>;

/// Owned URL record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebUrl {
    inner: Url,
}

impl WebUrl {
    /// Parse a URL, optionally relative to `base`.
    pub fn parse(input: &str, base: Option<&WebUrl>) -> UrlResult<Self> {
        let inner = match base {
            Some(base) => base
                .inner
                .join(input)
                .map_err(|err| UrlError::Invalid(err.to_string()))?,
            None => Url::parse(input).map_err(|err| UrlError::Invalid(err.to_string()))?,
        };
        Ok(Self { inner })
    }

    /// Serialized URL.
    #[must_use]
    pub fn href(&self) -> String {
        self.inner.as_str().to_string()
    }

    /// URL protocol including trailing `:`.
    #[must_use]
    pub fn protocol(&self) -> String {
        format!("{}:", self.inner.scheme())
    }

    /// URL origin.
    #[must_use]
    pub fn origin(&self) -> String {
        self.inner.origin().ascii_serialization()
    }

    /// Host plus optional port.
    #[must_use]
    pub fn host(&self) -> String {
        self.inner.host_str().map_or_else(String::new, |host| {
            if let Some(port) = self.inner.port() {
                format!("{host}:{port}")
            } else {
                host.to_string()
            }
        })
    }

    /// Pathname.
    #[must_use]
    pub fn pathname(&self) -> String {
        self.inner.path().to_string()
    }

    /// Query including leading `?`, or empty string.
    #[must_use]
    pub fn search(&self) -> String {
        self.inner
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default()
    }

    /// Fragment including leading `#`, or empty string.
    #[must_use]
    pub fn hash(&self) -> String {
        self.inner
            .fragment()
            .map(|hash| format!("#{hash}"))
            .unwrap_or_default()
    }

    /// Mutate pathname.
    pub fn set_pathname(&mut self, pathname: &str) {
        self.inner.set_path(pathname);
    }

    /// Mutate query. Accepts either `x=1` or `?x=1`; empty clears it.
    pub fn set_search(&mut self, search: &str) {
        let query = search.strip_prefix('?').unwrap_or(search);
        if query.is_empty() {
            self.inner.set_query(None);
        } else {
            self.inner.set_query(Some(query));
        }
    }

    /// Mutate fragment. Accepts either `x` or `#x`; empty clears it.
    pub fn set_hash(&mut self, hash: &str) {
        let fragment = hash.strip_prefix('#').unwrap_or(hash);
        if fragment.is_empty() {
            self.inner.set_fragment(None);
        } else {
            self.inner.set_fragment(Some(fragment));
        }
    }
}

/// Static URL class spec.
pub static URL_CLASS_SPEC: ClassSpec = ClassSpec {
    constructor: ConstructorSpec {
        name: "URL",
        length: 1,
        call: NativeCall::Static(url_constructor_native),
        static_methods: &[],
        prototype_methods: &[MethodSpec {
            name: "toString",
            length: 0,
            attrs: Attr::builtin_function(),
            call: NativeCall::Static(url_to_string_native),
        }],
        attrs: Attr::global_binding(),
    },
    prototype_accessors: &[],
};

fn url_constructor_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let input = crate::arg_string(args, 0);
    let base = match args.get(1) {
        Some(Value::String(value)) => Some(
            WebUrl::parse(&value.to_lossy_string(), None)
                .map_err(|err| crate::type_error("URL", err.to_string()))?,
        ),
        Some(Value::Undefined) | None => None,
        Some(value) => Some(
            WebUrl::parse(&value.display_string(), None)
                .map_err(|err| crate::type_error("URL", err.to_string()))?,
        ),
    };
    let url = WebUrl::parse(&input, base.as_ref())
        .map_err(|err| crate::type_error("URL", err.to_string()))?;
    url_object(ctx, url)
}

fn url_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    match ctx.this_value().clone() {
        Value::Object(object) => Ok(object),
        _ => Err(crate::type_error(name, "invalid URL receiver")),
    }
}

fn url_state<R>(
    ctx: &NativeCtx<'_>,
    name: &'static str,
    f: impl FnOnce(&WebUrl) -> R,
) -> Result<R, NativeError> {
    let object = url_receiver(ctx, name)?;
    object::with_host_data::<WebUrl, _>(object, ctx.heap(), f)
        .map_err(|err| crate::type_error(name, err.to_string()))
}

fn url_to_string_native(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let href = url_state(ctx, "URL.prototype.toString", WebUrl::href)?;
    crate::string_value(ctx, &href)
}

pub(crate) fn url_object(ctx: &mut NativeCtx<'_>, state: WebUrl) -> Result<Value, NativeError> {
    let href = crate::string_value(ctx, &state.href())?;
    let protocol = crate::string_value(ctx, &state.protocol())?;
    let origin = crate::string_value(ctx, &state.origin())?;
    let host = crate::string_value(ctx, &state.host())?;
    let pathname = crate::string_value(ctx, &state.pathname())?;
    let search = crate::string_value(ctx, &state.search())?;
    let hash = crate::string_value(ctx, &state.hash())?;
    let object = object::alloc_host_object(ctx.interp_mut().gc_heap_mut(), state)?;
    let mut builder = ObjectBuilder::from_object(ctx.interp_mut().gc_heap_mut(), object);
    builder
        .method(
            "toString",
            0,
            NativeCall::Static(url_to_string_native),
            Attr::builtin_function(),
        )
        .and_then(|builder| builder.property("href", href, Attr::data()))
        .and_then(|builder| builder.property("protocol", protocol, Attr::data()))
        .and_then(|builder| builder.property("origin", origin, Attr::data()))
        .and_then(|builder| builder.property("host", host, Attr::data()))
        .and_then(|builder| builder.property("pathname", pathname, Attr::data()))
        .and_then(|builder| builder.property("search", search, Attr::data()))
        .and_then(|builder| builder.property("hash", hash, Attr::data()))
        .map_err(|err| crate::type_error("URL", err.to_string()))?;
    let object = builder.build();
    Ok(Value::Object(object))
}
