//! WHATWG URL host-side record.

use otter_runtime::{
    RuntimeAttr as Attr, RuntimeJsObject as JsObject, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeValue as Value, runtime_optional_arg_to_string,
    runtime_this_object, runtime_with_host_data,
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

otter_macros::couch! {
    name = "URL",
    feature = WEB,
    constructor = (length = 1, call = url_constructor_native),
    prototype = {
        methods = {
            "toString" / 0 => url_to_string_native,
        },
    },
}

fn url_constructor_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let input = crate::arg_string(args, 0, ctx.heap());
    let base = runtime_optional_arg_to_string(args, 1, ctx.heap())
        .map(|value| WebUrl::parse(&value, None))
        .transpose()
        .map_err(|err| crate::type_error("URL", err.to_string()))?;
    let url = WebUrl::parse(&input, base.as_ref())
        .map_err(|err| crate::type_error("URL", err.to_string()))?;
    url_object(ctx, url)
}

fn url_receiver(ctx: &NativeCtx<'_>, name: &'static str) -> Result<JsObject, NativeError> {
    runtime_this_object(ctx, name, "URL")
}

fn url_state<R>(
    ctx: &NativeCtx<'_>,
    name: &'static str,
    f: impl FnOnce(&WebUrl) -> R,
) -> Result<R, NativeError> {
    let object = url_receiver(ctx, name)?;
    runtime_with_host_data::<WebUrl, _>(ctx, object, f)
        .map_err(|err| crate::type_error(name, err.to_string()))
}

fn url_to_string_native(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let href = url_state(ctx, "URL.prototype.toString", WebUrl::href)?;
    crate::string_value(ctx, &href)
}

pub(crate) fn url_object(ctx: &mut NativeCtx<'_>, state: WebUrl) -> Result<Value, NativeError> {
    // Snapshot the string fields as Rust values before moving `state` into the
    // host object; the JS strings are minted inside the scope, each right
    // before its define, so no unrooted JsString local is held across another
    // allocation.
    let href = state.href();
    let protocol = state.protocol();
    let origin = state.origin();
    let host = state.host();
    let pathname = state.pathname();
    let search = state.search();
    let hash = state.hash();
    ctx.scope(|ctx, s| {
        let obj = ctx.scoped_host_object(s, state)?;
        let to_string = ctx.scoped_native_method(s, "toString", 0, url_to_string_native)?;
        ctx.scoped_define_data(
            s,
            obj,
            "toString",
            to_string,
            Attr::builtin_function().to_flags(),
        )?;
        let data = Attr::data().to_flags();
        for (name, text) in [
            ("href", &href),
            ("protocol", &protocol),
            ("origin", &origin),
            ("host", &host),
            ("pathname", &pathname),
            ("search", &search),
            ("hash", &hash),
        ] {
            let value = ctx.scoped_string(s, text)?;
            ctx.scoped_define_data(s, obj, name, value, data)?;
        }
        Ok::<Value, NativeError>(ctx.escape(obj))
    })
    .inspect(|value| {
        // Link the instance to URL.prototype so `instanceof URL` holds and
        // prototype methods are inherited. (The snapshot data properties above
        // are replaced by live accessors in a later slice.)
        crate::link_class_prototype(ctx, *value, "URL");
    })
}
