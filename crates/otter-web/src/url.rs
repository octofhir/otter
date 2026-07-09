//! WHATWG URL host class.
//!
//! Declared through `#[js_class]`: the URL record lives in Rust
//! (`url::Url` under the hood), and every JS-visible part is a live
//! prototype accessor over that record — reads serialize the current
//! state, writes mutate it, so `u.pathname = "/x"` is immediately
//! visible through `u.href`. Setter parse failures are silently
//! ignored where the URL Standard says so (`protocol`, `host`);
//! assigning an unparsable `href` throws `TypeError` per spec.
//!
//! # Contents
//! - [`WebUrl`] — the owned URL record (also the Rust-side API).
//! - [`UrlError`] — parse/mutation failures.
//!
//! # Invariants
//! - The instance holds only host data; all members live on
//!   `URL.prototype` (accessors + `toString`/`toJSON`).
//! - Serialization always reflects the current record — there are no
//!   snapshot properties to go stale.
//!
//! # See also
//! - <https://url.spec.whatwg.org/#url-class>

use otter_macros::{HostClass, js_class};
use otter_runtime::marshal::{JsError, USVString};
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
#[derive(Debug, Clone, PartialEq, Eq, HostClass)]
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

    /// Host without the port.
    #[must_use]
    pub fn hostname(&self) -> String {
        self.inner.host_str().unwrap_or_default().to_string()
    }

    /// Explicit port, or empty string.
    #[must_use]
    pub fn port(&self) -> String {
        self.inner
            .port()
            .map(|port| port.to_string())
            .unwrap_or_default()
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

    /// Replace the whole record by reparsing `href`.
    pub fn set_href(&mut self, href: &str) -> UrlResult<()> {
        self.inner = Url::parse(href).map_err(|err| UrlError::Invalid(err.to_string()))?;
        Ok(())
    }

    /// Mutate the scheme. Accepts either `https` or `https:`; a
    /// rejected scheme leaves the record unchanged (spec: basic URL
    /// parse failure in the protocol setter is ignored).
    pub fn set_protocol(&mut self, protocol: &str) {
        let scheme = protocol.strip_suffix(':').unwrap_or(protocol);
        let _ = self.inner.set_scheme(scheme);
    }

    /// Mutate host (and optional port). A rejected host is ignored
    /// per the URL Standard's host-setter semantics.
    pub fn set_host(&mut self, host: &str) {
        if host.is_empty() {
            let _ = self.inner.set_host(None);
            return;
        }
        let (name, port) = match host.rsplit_once(':') {
            Some((name, port)) if !port.is_empty() => match port.parse::<u16>() {
                Ok(port) => (name, Some(port)),
                Err(_) => (host, None),
            },
            _ => (host, None),
        };
        if self.inner.set_host(Some(name)).is_ok()
            && let Some(port) = port
        {
            let _ = self.inner.set_port(Some(port));
        }
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

#[js_class(name = "URL", feature = WEB, js = "url.class.js")]
impl WebUrl {
    #[constructor]
    fn js_new(input: USVString, base: Option<USVString>) -> Result<WebUrl, JsError> {
        let base = base
            .map(|base| WebUrl::parse(base.as_str(), None))
            .transpose()
            .map_err(|err| JsError::Type(err.to_string()))?;
        WebUrl::parse(input.as_str(), base.as_ref()).map_err(|err| JsError::Type(err.to_string()))
    }

    #[getter(name = "href")]
    fn js_href(&self) -> String {
        self.href()
    }

    #[setter(name = "href")]
    fn js_set_href(&mut self, value: USVString) -> Result<(), JsError> {
        self.set_href(value.as_str())
            .map_err(|err| JsError::Type(err.to_string()))
    }

    #[getter(name = "origin")]
    fn js_origin(&self) -> String {
        self.origin()
    }

    #[getter(name = "protocol")]
    fn js_protocol(&self) -> String {
        self.protocol()
    }

    #[setter(name = "protocol")]
    fn js_set_protocol(&mut self, value: USVString) {
        self.set_protocol(value.as_str());
    }

    #[getter(name = "host")]
    fn js_host(&self) -> String {
        self.host()
    }

    #[setter(name = "host")]
    fn js_set_host(&mut self, value: USVString) {
        self.set_host(value.as_str());
    }

    #[getter(name = "hostname")]
    fn js_hostname(&self) -> String {
        self.hostname()
    }

    #[getter(name = "port")]
    fn js_port(&self) -> String {
        self.port()
    }

    #[getter(name = "pathname")]
    fn js_pathname(&self) -> String {
        self.pathname()
    }

    #[setter(name = "pathname")]
    fn js_set_pathname(&mut self, value: USVString) {
        self.set_pathname(value.as_str());
    }

    #[getter(name = "search")]
    fn js_search(&self) -> String {
        self.search()
    }

    #[setter(name = "search")]
    fn js_set_search(&mut self, value: USVString) {
        self.set_search(value.as_str());
    }

    #[getter(name = "hash")]
    fn js_hash(&self) -> String {
        self.hash()
    }

    #[setter(name = "hash")]
    fn js_set_hash(&mut self, value: USVString) {
        self.set_hash(value.as_str());
    }

    #[method(name = "toString")]
    fn js_to_string(&self) -> String {
        self.href()
    }

    #[method(name = "toJSON")]
    fn js_to_json(&self) -> String {
        self.href()
    }
}
