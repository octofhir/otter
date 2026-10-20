//! Outbound HTTP client: HTTP/1.1 and HTTP/2 over TCP or rustls with the
//! bundled trust roots, response decompression and redirect following.
//!
//! # Contents
//! - [`Client`] — a pooled client; cheap to clone.
//! - [`ClientBuilder`] — user agent and redirect handling for a client.
//! - [`RequestBuilder`] — one request: headers, body, timeout.
//! - [`Response`] — status, headers, final URL and a streamed body.
//! - [`Redirects`] — whether and how a client follows 3xx responses.
//! - [`Error`] — a transport failure with its cause chain.
//!
//! # Invariants
//! - Trust is the bundled Mozilla root set plus the certificates the
//!   `NODE_EXTRA_CA_CERTS` file names: the default store node and bun ship.
//!   No platform trust framework is linked, so a launch loads none.
//! - The connector, TLS configuration and pool are built on a client's first
//!   request, not when the client is made.
//! - A body is decoded only when the client chose the encoding: a request
//!   that names its own `accept-encoding` or a `range` reads the bytes as
//!   sent. A decoded response drops `content-encoding` and `content-length`.
//! - A followed redirect is checked by the client's hook before the next
//!   connection. 303 (except after HEAD), and 301/302 after POST, continue as
//!   a bodiless GET; a hop to another origin drops the credential headers.
//! - A timeout bounds the whole exchange, body reads included.
//!
//! # See also
//! - `otter_runtime::web_fetch_host` — `fetch` over this client.
//! - `otter_pm::registry` / `otter_pm::tarball` — registry traffic.

use std::io::Write as _;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use http_body_util::{BodyExt as _, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client as Pool;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject as _;
use tokio::time::Instant;

pub use http::{Method, StatusCode, header};
pub use url::Url;

/// Redirect hops one request follows before it fails, as fetch allows.
const MAX_REDIRECTS: usize = 20;

/// Decides whether a redirect may go on: called with the next URL and the
/// URL that answered with the redirect; `Err` carries the refusal.
pub type RedirectCheck = Arc<dyn Fn(&Url, &Url) -> Result<(), String> + Send + Sync>;

/// How a client treats a 3xx response with a `location`.
#[derive(Clone)]
pub enum Redirects {
    /// Hand the 3xx back to the caller.
    Stop,
    /// Follow it, asking the check first when there is one.
    Follow(Option<RedirectCheck>),
}

type Connection = Pool<HttpsConnector<HttpConnector>, Full<Bytes>>;

struct Inner {
    redirects: Redirects,
    user_agent: Option<HeaderValue>,
    pool: OnceLock<Connection>,
}

/// A pooled HTTP client. Clones share one connection pool.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field(
                "follows_redirects",
                &matches!(self.inner.redirects, Redirects::Follow(_)),
            )
            .field("connected", &self.inner.pool.get().is_some())
            .finish()
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// Settings for a [`Client`].
pub struct ClientBuilder {
    redirects: Redirects,
    user_agent: Option<HeaderValue>,
}

impl ClientBuilder {
    /// Send this `user-agent` on requests that name none.
    #[must_use]
    pub fn user_agent(mut self, agent: &str) -> Self {
        self.user_agent = HeaderValue::from_str(agent).ok();
        self
    }

    /// Choose how 3xx responses are treated.
    #[must_use]
    pub fn redirects(mut self, redirects: Redirects) -> Self {
        self.redirects = redirects;
        self
    }

    /// The client.
    #[must_use]
    pub fn build(self) -> Client {
        Client {
            inner: Arc::new(Inner {
                redirects: self.redirects,
                user_agent: self.user_agent,
                pool: OnceLock::new(),
            }),
        }
    }
}

/// A value a request can be addressed to.
pub trait IntoUrl {
    /// The parsed URL.
    fn into_url(self) -> Result<Url, url::ParseError>;
}

impl IntoUrl for Url {
    fn into_url(self) -> Result<Url, url::ParseError> {
        Ok(self)
    }
}

impl IntoUrl for &str {
    fn into_url(self) -> Result<Url, url::ParseError> {
        Url::parse(self)
    }
}

impl IntoUrl for &String {
    fn into_url(self) -> Result<Url, url::ParseError> {
        Url::parse(self)
    }
}

impl IntoUrl for String {
    fn into_url(self) -> Result<Url, url::ParseError> {
        Url::parse(&self)
    }
}

impl Client {
    /// A client that follows redirects and sends no default user agent.
    #[must_use]
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Settings for a new client; redirects are followed unless changed.
    #[must_use]
    pub fn builder() -> ClientBuilder {
        ClientBuilder {
            redirects: Redirects::Follow(None),
            user_agent: None,
        }
    }

    /// A GET request.
    pub fn get(&self, url: impl IntoUrl) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    /// A POST request.
    pub fn post(&self, url: impl IntoUrl) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// A request with any method.
    pub fn request(&self, method: Method, url: impl IntoUrl) -> RequestBuilder {
        let url = url
            .into_url()
            .map_err(|error| Error::new("invalid URL", error));
        RequestBuilder {
            client: self.clone(),
            method,
            url,
            headers: HeaderMap::new(),
            body: None,
            timeout: None,
            error: None,
        }
    }

    fn pool(&self) -> &Connection {
        self.inner.pool.get_or_init(|| {
            let connector = hyper_rustls::HttpsConnectorBuilder::new()
                .with_tls_config(tls_config())
                .https_or_http()
                .enable_http1()
                .enable_http2()
                .build();
            Pool::builder(TokioExecutor::new()).build(connector)
        })
    }
}

/// The bundled roots plus the `NODE_EXTRA_CA_CERTS` file's certificates.
fn tls_config() -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = std::env::var_os("NODE_EXTRA_CA_CERTS") {
        match CertificateDer::pem_file_iter(&path) {
            Ok(certificates) => {
                for certificate in certificates {
                    match certificate {
                        Ok(certificate) => {
                            if let Err(error) = roots.add(certificate) {
                                extra_certs_warning(&path, &error);
                            }
                        }
                        Err(error) => extra_certs_warning(&path, &error),
                    }
                }
            }
            Err(error) => extra_certs_warning(&path, &error),
        }
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the bundled provider supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

fn extra_certs_warning(path: &std::ffi::OsStr, error: &dyn std::fmt::Display) {
    eprintln!(
        "Warning: Ignoring extra certs from `{}`, load failed: {error}",
        path.to_string_lossy()
    );
}

/// One request being assembled.
pub struct RequestBuilder {
    client: Client,
    method: Method,
    url: Result<Url, Error>,
    headers: HeaderMap,
    body: Option<Bytes>,
    timeout: Option<Duration>,
    error: Option<Error>,
}

impl RequestBuilder {
    /// Append a header.
    #[must_use]
    pub fn header<K, V>(mut self, name: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        let name = HeaderName::try_from(name).map_err(Into::into);
        let value = HeaderValue::try_from(value).map_err(Into::into);
        match (name, value) {
            (Ok(name), Ok(value)) => {
                self.headers.append(name, value);
            }
            (Err(error), _) | (_, Err(error)) => {
                self.error
                    .get_or_insert(Error::new("invalid header", error));
            }
        }
        self
    }

    /// The request body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// A JSON body, typed `application/json` unless a type is named.
    #[must_use]
    pub fn json<T: serde::Serialize + ?Sized>(mut self, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => {
                self.headers
                    .entry(header::CONTENT_TYPE)
                    .or_insert(HeaderValue::from_static("application/json"));
                self.body = Some(body.into());
            }
            Err(error) => {
                self.error
                    .get_or_insert(Error::new("request body is not JSON", error));
            }
        }
        self
    }

    /// Bound the whole exchange, body reads included.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Send the request, following redirects as the client says.
    pub async fn send(self) -> Result<Response, Error> {
        let deadline = self.timeout.map(|timeout| Instant::now() + timeout);
        match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, self.exchange(Some(deadline)))
                .await
                .map_err(|_| Error::timeout())?,
            None => self.exchange(None).await,
        }
    }

    async fn exchange(self, deadline: Option<Instant>) -> Result<Response, Error> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let mut url = self.url?;
        let mut method = self.method;
        let mut body = self.body;
        let mut headers = self.headers;
        let inner = &self.client.inner;
        if !headers.contains_key(header::USER_AGENT)
            && let Some(agent) = &inner.user_agent
        {
            headers.insert(header::USER_AGENT, agent.clone());
        }
        let decode =
            !headers.contains_key(header::ACCEPT_ENCODING) && !headers.contains_key(header::RANGE);
        if decode {
            headers.insert(
                header::ACCEPT_ENCODING,
                HeaderValue::from_static("gzip, deflate"),
            );
        }
        let mut hops = 0;
        loop {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(Error::message(format!(
                    "URL scheme is not supported: {url}"
                )));
            }
            let mut request = http::Request::new(Full::new(body.clone().unwrap_or_default()));
            *request.method_mut() = method.clone();
            *request.uri_mut() = url
                .as_str()
                .parse()
                .map_err(|error| Error::new("invalid URL", error))?;
            *request.headers_mut() = headers.clone();
            let response = self.client.pool().request(request).await.map_err(|error| {
                Error::new(format!("error sending request for url ({url})"), error)
            })?;
            let status = response.status();
            if let Redirects::Follow(check) = &inner.redirects
                && status.is_redirection()
                && status != StatusCode::NOT_MODIFIED
                && let Some(location) = response.headers().get(header::LOCATION)
            {
                let next = location
                    .to_str()
                    .ok()
                    .and_then(|location| url.join(location).ok())
                    .ok_or_else(|| {
                        Error::message(format!(
                            "error following redirect for url ({url}): invalid location"
                        ))
                    })?;
                hops += 1;
                if hops > MAX_REDIRECTS {
                    return Err(Error::message(format!(
                        "error following redirect for url ({url}): too many redirects"
                    )));
                }
                if let Some(check) = check {
                    check(&next, &url).map_err(Error::message)?;
                }
                let to_get = (status == StatusCode::SEE_OTHER && method != Method::HEAD)
                    || (matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND)
                        && method == Method::POST);
                if to_get {
                    method = Method::GET;
                    body = None;
                    for name in [
                        header::CONTENT_TYPE,
                        header::CONTENT_LENGTH,
                        header::CONTENT_ENCODING,
                        header::CONTENT_LANGUAGE,
                        header::CONTENT_LOCATION,
                    ] {
                        headers.remove(name);
                    }
                }
                if next.origin() != url.origin() {
                    for name in [
                        header::AUTHORIZATION,
                        header::COOKIE,
                        header::PROXY_AUTHORIZATION,
                    ] {
                        headers.remove(name);
                    }
                }
                url = next;
                continue;
            }
            let (mut parts, incoming) = response.into_parts();
            let decoder = if decode {
                Decoder::for_encoding(parts.headers.get(header::CONTENT_ENCODING))
            } else {
                Decoder::Identity
            };
            if !matches!(decoder, Decoder::Identity) {
                parts.headers.remove(header::CONTENT_ENCODING);
                parts.headers.remove(header::CONTENT_LENGTH);
            }
            return Ok(Response {
                status,
                headers: parts.headers,
                url,
                redirected: hops > 0,
                incoming: Some(incoming),
                decoder,
                deadline,
            });
        }
    }
}

enum Decoder {
    Identity,
    Gzip(flate2::write::GzDecoder<Vec<u8>>),
    Deflate(flate2::write::ZlibDecoder<Vec<u8>>),
}

impl Decoder {
    fn for_encoding(encoding: Option<&HeaderValue>) -> Self {
        let encoding = encoding
            .and_then(|value| value.to_str().ok())
            .map(str::trim);
        match encoding {
            Some(name)
                if name.eq_ignore_ascii_case("gzip") || name.eq_ignore_ascii_case("x-gzip") =>
            {
                Self::Gzip(flate2::write::GzDecoder::new(Vec::new()))
            }
            Some(name) if name.eq_ignore_ascii_case("deflate") => {
                Self::Deflate(flate2::write::ZlibDecoder::new(Vec::new()))
            }
            _ => Self::Identity,
        }
    }

    fn feed(&mut self, data: Bytes) -> std::io::Result<Bytes> {
        match self {
            Self::Identity => Ok(data),
            Self::Gzip(decoder) => {
                decoder.write_all(&data)?;
                Ok(std::mem::take(decoder.get_mut()).into())
            }
            Self::Deflate(decoder) => {
                decoder.write_all(&data)?;
                Ok(std::mem::take(decoder.get_mut()).into())
            }
        }
    }

    fn finish(&mut self) -> std::io::Result<Bytes> {
        match self {
            Self::Identity => Ok(Bytes::new()),
            Self::Gzip(decoder) => {
                decoder.try_finish()?;
                Ok(std::mem::take(decoder.get_mut()).into())
            }
            Self::Deflate(decoder) => {
                decoder.try_finish()?;
                Ok(std::mem::take(decoder.get_mut()).into())
            }
        }
    }
}

/// A response: its head now, its body as it is read.
pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    url: Url,
    redirected: bool,
    incoming: Option<hyper::body::Incoming>,
    decoder: Decoder,
    deadline: Option<Instant>,
}

impl Response {
    /// The status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The headers, in wire order.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The URL that answered, after redirects.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Whether a redirect was followed to reach [`Response::url`].
    #[must_use]
    pub fn redirected(&self) -> bool {
        self.redirected
    }

    /// The next decoded body chunk; `None` once the body ended.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, Error> {
        loop {
            let Some(incoming) = self.incoming.as_mut() else {
                return Ok(None);
            };
            let frame = match self.deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, incoming.frame())
                    .await
                    .map_err(|_| Error::timeout())?,
                None => incoming.frame().await,
            };
            match frame {
                None => {
                    self.incoming = None;
                    let tail = self.decoder.finish().map_err(Error::decode)?;
                    return Ok((!tail.is_empty()).then_some(tail));
                }
                Some(Err(error)) => {
                    self.incoming = None;
                    return Err(Error::new("error reading response body", error));
                }
                Some(Ok(frame)) => {
                    let Ok(data) = frame.into_data() else {
                        continue;
                    };
                    let out = self.decoder.feed(data).map_err(Error::decode)?;
                    if !out.is_empty() {
                        return Ok(Some(out));
                    }
                }
            }
        }
    }

    /// The whole body.
    pub async fn bytes(mut self) -> Result<Bytes, Error> {
        let mut first: Option<Bytes> = None;
        let mut rest: Vec<u8> = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            match &first {
                None => first = Some(chunk),
                Some(head) => {
                    if rest.is_empty() {
                        rest.extend_from_slice(head);
                    }
                    rest.extend_from_slice(&chunk);
                }
            }
        }
        Ok(if rest.is_empty() {
            first.unwrap_or_default()
        } else {
            rest.into()
        })
    }

    /// The whole body as text; invalid UTF-8 becomes U+FFFD.
    pub async fn text(self) -> Result<String, Error> {
        let bytes = self.bytes().await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The whole body parsed as JSON.
    pub async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T, Error> {
        let bytes = self.bytes().await?;
        serde_json::from_slice(&bytes)
            .map_err(|error| Error::new("error decoding response body", error))
    }
}

/// A failed request, with the cause that failed it.
#[derive(Debug)]
pub struct Error {
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    timeout: bool,
}

impl Error {
    fn new(
        message: impl Into<String>,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(source.into()),
            timeout: false,
        }
    }

    fn message(message: String) -> Self {
        Self {
            message,
            source: None,
            timeout: false,
        }
    }

    fn timeout() -> Self {
        Self {
            message: "operation timed out".to_string(),
            source: None,
            timeout: true,
        }
    }

    fn decode(error: std::io::Error) -> Self {
        Self::new("error decoding response body", error)
    }

    /// Whether the request ran out of time.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        self.timeout
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}
