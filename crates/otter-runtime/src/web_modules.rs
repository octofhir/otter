//! Embedder-delivered ES module graphs for browser hosts.
//!
//! A browser cannot let its script engine fetch. The network, its cache,
//! redirects, MIME checks and request limits belong to the embedder, and the
//! thread a page's scripts run on must never wait on a socket. A
//! [`WebModuleMap`] is where the two meet: the embedder registers the
//! document's import map and delivers module sources into the map from its
//! own network, and the engine resolves, discovers, compiles and runs graphs
//! only from what was delivered.
//!
//! Loading a graph is two-phase and never blocks:
//!
//! 1. Prepare. [`WebModuleMap::graph_status`] walks the graph rooted at an
//!    entry the embedder delivered, parsing each delivered module once for
//!    its static requests and resolving them through the import map. It
//!    answers [`WebModuleGraph::Pending`] with the URLs that nobody has asked
//!    for yet (they are marked requested, so each is reported once), or
//!    [`WebModuleGraph::Ready`] once every module is present.
//! 2. Deliver. The embedder fetches those URLs however it likes and hands
//!    each result to [`WebModuleMap::deliver`], with the final URL after
//!    redirects, and repeats step 1.
//! 3. Run. Once ready, [`crate::Runtime::run_module_source`] compiles, links
//!    and evaluates the graph from the delivered sources.
//!
//! Dynamic `import()` takes the same path: the runtime parks the request,
//! [`crate::Runtime::web_module_fetches`] reports what it still needs, and
//! [`crate::Runtime::resume_web_dynamic_imports`] settles the promise once
//! the embedder delivered it.
//!
//! # Contents
//! - [`WebModuleMap`] — one document's module map, import map and base URL.
//! - [`WebModuleSource`] — one delivered response.
//! - [`WebModuleGraph`] — the answer of the prepare phase.
//! - [`WebModuleError`] — a fetch, parse or resolution failure in a graph.
//! - [`WebModuleCodeCache`] — a module's compiled form, for the embedder to
//!   keep beside the response ([`WebModuleMap::produce_code_caches`],
//!   `Runtime::web_module_code_caches`, [`WebModuleMap::deliver_cached`]).
//! - [`PreparedWebModule`] — a response parsed and compiled, or read back from
//!   its code cache, on any thread ([`WebModuleMap::prepare`]) and delivered
//!   with that work done ([`WebModuleMap::deliver_prepared`]).
//!
//! # Invariants
//! - The map is keyed by request URL and holds each response once, so two
//!   graphs, or two `import()` calls, that request the same URL share one
//!   fetch and one source (the HTML module map).
//! - A delivered module is parsed once. The walk that finds its requests —
//!   or [`WebModuleMap::prepare`], before delivery — also compiles it against
//!   the targets they resolve to; the graph that runs it takes that compile
//!   while its imports still resolve the same, and parses again only when
//!   they do not.
//! - A module delivered with a code cache for its exact text, URL and this
//!   engine build is not parsed at all: the walk reads its requests and its
//!   compile from the bytes, which pass the bytecode verifier. Bytes that do
//!   not match are ignored and the module compiles as without them.
//! - Every module in a web graph is an ES module in JavaScript, whatever its
//!   URL looks like: there is no CommonJS detection and no TypeScript.
//! - Literal `import("…")` targets are not part of a static graph: they are
//!   fetched when the call runs, as browsers do.
//! - The engine never touches the filesystem or network for a web graph.
//!
//! # See also
//! - [`crate::import_map`] — specifier resolution.
//! - <https://html.spec.whatwg.org/multipage/webappapis.html#fetching-scripts>

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use url::Url;

use crate::import_map::{ImportMap, ImportMapResolveError};
use crate::module_graph::{PrecompiledModule, PrefetchRequest, scan_web_module};

/// The fragment that keeps inline module scripts apart in the module map.
///
/// Every inline module script of a document has the document's base URL as
/// its URL, but each is its own module record. The map keys each one by the
/// base URL plus this marker and an ordinal; `import.meta.url` and specifier
/// resolution strip it again.
pub const INLINE_MODULE_MARKER: &str = otter_vm::INLINE_MODULE_MARKER;

/// One delivered module response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebModuleSource {
    /// The URL the response came from, after redirects. Relative requests in
    /// the module resolve against it and it is the module's `import.meta.url`.
    pub final_url: String,
    /// The response's `Content-Type`, when known. The embedder has already
    /// checked that it is a JavaScript MIME type.
    pub content_type: Option<String>,
    /// The decoded module source.
    pub text: String,
}

/// Where a graph stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebModuleGraph {
    /// Every module of the graph was delivered.
    Ready,
    /// Modules are still outstanding. `fetch` lists the ones nobody asked for
    /// before this call; the others are already on their way.
    Pending {
        /// Request URLs the embedder should fetch now.
        fetch: Vec<String>,
    },
}

/// A graph that cannot be run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WebModuleError {
    /// A specifier did not resolve (a `TypeError` in the page).
    #[error("{message}")]
    Resolve {
        /// The module that made the request.
        referrer: String,
        /// The reason, naming the specifier.
        message: String,
    },
    /// A module's fetch failed (an `error` event on the script element).
    #[error("failed to load module {url}: {message}")]
    Fetch {
        /// The request URL.
        url: String,
        /// What the embedder reported.
        message: String,
    },
    /// A module does not parse (a `SyntaxError` in the page).
    #[error("{message}")]
    Parse {
        /// The module URL.
        url: String,
        /// The parser's diagnostic.
        message: String,
    },
}

/// One document's module map, shared between the embedder and its runtime.
///
/// Cheap to clone; clones share one map. `Send` and `Sync`, so the embedder
/// may deliver from whichever thread owns its network results.
#[derive(Clone, Default)]
pub struct WebModuleMap {
    state: Arc<RwLock<WebModuleState>>,
}

#[derive(Default)]
struct WebModuleState {
    import_map: ImportMap,
    /// The document's base URL: what a request from a classic script, or
    /// from code with no module behind it, resolves against.
    document_base: Option<Url>,
    slots: HashMap<String, Slot>,
    /// Parses of delivered sources, by walks and by graphs.
    parses: u64,
    /// The embedder keeps code caches: fresh compiles are encoded for it.
    produce_code_caches: bool,
    /// Code caches encoded since the embedder last took them.
    fresh_code_caches: Vec<WebModuleCodeCache>,
    /// Modules whose first-call bodies the interpreter is to keep, by the
    /// URL they compiled as, until the runtime hands them over.
    kept: Vec<(String, otter_vm::KeptScript)>,
    /// Every module whose bodies are kept: the URL it compiled as and the
    /// one it was requested as.
    kept_urls: Vec<(String, String)>,
}

/// The compiled form of one delivered module, for the embedder to keep
/// beside the response and hand back with it on a later load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebModuleCodeCache {
    /// The URL the module was requested as.
    pub request_url: String,
    /// The bytes [`WebModuleMap::deliver_cached`] takes back. They are this
    /// engine's own: valid only for the same engine build, module text and
    /// URL, and checked for exactly that before use.
    pub bytes: Vec<u8>,
}

/// One delivered response parsed and compiled — or read back from the code
/// cache an earlier load produced for it — by [`WebModuleMap::prepare`] on
/// any thread, for [`WebModuleMap::deliver_prepared`] to put in the map.
///
/// A browser has a module's bytes before its graph runs: preparing it as they
/// arrive takes the parse and compile off the thread that runs the page.
pub struct PreparedWebModule {
    source: WebModuleSource,
    /// The module's static requests and its compile, or the parse diagnostic.
    scan: Result<crate::module_graph::WebModuleScan, String>,
    /// The first-call bodies of a compile read back from a code cache.
    kept: Option<otter_vm::KeptScript>,
}

const _: fn() = || {
    fn send<T: Send>() {}
    send::<PreparedWebModule>();
};

enum Slot {
    Requested,
    Delivered(Box<Delivered>),
    Failed(String),
}

struct Delivered {
    source: WebModuleSource,
    /// Static requests, parsed once on first need. `Err` holds the parse
    /// diagnostic.
    requests: Option<Result<Vec<PrefetchRequest>, String>>,
    /// The compile made by that parse, until a graph takes it.
    compiled: Option<PrecompiledModule>,
    /// Code cache the embedder delivered with the response, until a walk
    /// uses or rejects it.
    code_cache: Option<Vec<u8>>,
}

impl std::fmt::Debug for WebModuleMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let modules = self.read().slots.len();
        f.debug_struct("WebModuleMap")
            .field("modules", &modules)
            .finish_non_exhaustive()
    }
}

impl WebModuleMap {
    /// An empty map with no import map and no base URL.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, WebModuleState> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, WebModuleState> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set the document's base URL, against which requests from classic
    /// scripts resolve.
    pub fn set_document_base(&self, url: &str) {
        self.write().document_base = Url::parse(strip_inline_marker(url)).ok();
    }

    /// Register a `<script type="importmap">`, merging it into any map the
    /// document already has. Returns the warnings for entries it ignored.
    ///
    /// # Errors
    /// The text is not a valid import map; the page reports it to its global.
    pub fn register_import_map(&self, json: &str, base_url: &str) -> Result<Vec<String>, String> {
        let (map, warnings) = ImportMap::parse(json, strip_inline_marker(base_url))?;
        self.write().import_map.merge(map);
        Ok(warnings)
    }

    /// The module map key of the `ordinal`th inline module script of a
    /// document whose base URL is `base_url`.
    #[must_use]
    pub fn inline_url(base_url: &str, ordinal: u64) -> String {
        let base = match Url::parse(base_url) {
            Ok(mut url) => {
                url.set_fragment(None);
                url.to_string()
            }
            Err(_) => base_url.to_owned(),
        };
        format!("{base}#{INLINE_MODULE_MARKER}{ordinal}")
    }

    /// Resolve `specifier` as requested by the module at `referrer`, or by
    /// the document when there is no module behind the request.
    ///
    /// # Errors
    /// The specifier is bare and unmapped, blocked by the import map, or the
    /// referrer has no usable base URL.
    pub fn resolve(
        &self,
        specifier: &str,
        referrer: Option<&str>,
    ) -> Result<String, ImportMapResolveError> {
        let state = self.read();
        let base = referrer
            .and_then(|referrer| Url::parse(strip_inline_marker(referrer)).ok())
            .or_else(|| state.document_base.clone())
            .ok_or_else(|| ImportMapResolveError {
                message: format!("cannot resolve \"{specifier}\": the document has no base URL"),
            })?;
        state
            .import_map
            .resolve(specifier, &base)
            .map(|url| url.to_string())
    }

    /// Record the result of fetching `request_url`. A second delivery for the
    /// same URL replaces nothing: the first response is the module.
    pub fn deliver(&self, request_url: &str, response: Result<WebModuleSource, String>) {
        self.deliver_cached(request_url, response, None);
    }

    /// [`Self::deliver`] with the code cache an earlier load produced for
    /// the same response (`Runtime::web_module_code_caches`). The module is then
    /// neither parsed nor compiled, provided the bytes match this engine
    /// build, the module text and its URL; otherwise they are ignored.
    pub fn deliver_cached(
        &self,
        request_url: &str,
        response: Result<WebModuleSource, String>,
        code_cache: Option<Vec<u8>>,
    ) {
        let mut state = self.write();
        if matches!(
            state.slots.get(request_url),
            Some(Slot::Delivered(_) | Slot::Failed(_))
        ) {
            return;
        }
        let slot = match response {
            Ok(source) => Slot::Delivered(Box::new(Delivered {
                source,
                requests: None,
                compiled: None,
                code_cache,
            })),
            Err(message) => Slot::Failed(message),
        };
        state.slots.insert(request_url.to_owned(), slot);
    }

    /// Parse and compile `response`, unless `code_cache` holds its compiled
    /// form, which is then decoded and verified instead. Runs on any thread;
    /// its static requests resolve through the import map the document has
    /// now, and a graph that finds them resolving differently later compiles
    /// the module again.
    #[must_use]
    pub fn prepare(
        &self,
        response: WebModuleSource,
        code_cache: Option<Vec<u8>>,
    ) -> PreparedWebModule {
        if let Some(bytes) = code_cache
            && let Some((cached, kept)) = crate::module_graph::decode_web_module_cache(
                &bytes,
                &response.text,
                &response.final_url,
            )
        {
            return PreparedWebModule {
                source: response,
                scan: Ok(cached),
                kept: Some(kept),
            };
        }
        let import_map = self.read().import_map.clone();
        let base = Url::parse(strip_inline_marker(&response.final_url)).ok();
        let scan = scan_requests(&response, |specifier| {
            import_map
                .resolve(specifier, base.as_ref()?)
                .ok()
                .map(|url| url.to_string())
        });
        PreparedWebModule {
            source: response,
            scan,
            kept: None,
        }
    }

    /// [`Self::deliver`] of a response [`Self::prepare`] already parsed and
    /// compiled, or read back from its code cache.
    pub fn deliver_prepared(&self, request_url: &str, prepared: PreparedWebModule) {
        let mut guard = self.write();
        let state = &mut *guard;
        if matches!(
            state.slots.get(request_url),
            Some(Slot::Delivered(_) | Slot::Failed(_))
        ) {
            return;
        }
        let PreparedWebModule { source, scan, kept } = prepared;
        let final_url = source.final_url.clone();
        let mut delivered = Box::new(Delivered {
            source,
            requests: None,
            compiled: None,
            code_cache: None,
        });
        let fresh = kept.is_none();
        match kept {
            Some(kept) => {
                state.kept.push((final_url.clone(), kept));
                state
                    .kept_urls
                    .push((final_url.clone(), request_url.to_owned()));
            }
            None => state.parses += 1,
        }
        delivered.requests = Some(scan.map(|scan| {
            delivered.compiled = scan.compiled;
            scan.requests
        }));
        if fresh {
            produce_code_cache(state, request_url, &final_url, &delivered);
        }
        state
            .slots
            .insert(request_url.to_owned(), Slot::Delivered(delivered));
    }

    /// Encode the compile of every module delivered without a usable code
    /// cache from now on, for `Runtime::web_module_code_caches`.
    pub fn produce_code_caches(&self) {
        self.write().produce_code_caches = true;
    }

    /// The code caches encoded since the last call, for
    /// `Runtime::web_module_code_caches` to hand the embedder.
    #[must_use]
    pub(crate) fn take_code_caches(&self) -> Vec<WebModuleCodeCache> {
        std::mem::take(&mut self.write().fresh_code_caches)
    }

    /// The modules whose first-call bodies the interpreter is to keep from
    /// now on, each once.
    pub(crate) fn take_kept(&self) -> Vec<(String, otter_vm::KeptScript)> {
        std::mem::take(&mut self.write().kept)
    }

    /// Every module whose first-call bodies are kept: the URL it compiled
    /// as and the one it was requested as.
    pub(crate) fn kept_urls(&self) -> Vec<(String, String)> {
        self.read().kept_urls.clone()
    }

    /// Whether `url` was delivered, successfully or not.
    #[must_use]
    pub fn is_settled(&self, url: &str) -> bool {
        matches!(
            self.read().slots.get(url),
            Some(Slot::Delivered(_) | Slot::Failed(_))
        )
    }

    /// The delivered source for `request_url`, if any.
    #[must_use]
    pub fn source(&self, request_url: &str) -> Option<WebModuleSource> {
        match self.read().slots.get(request_url)? {
            Slot::Delivered(delivered) => Some(delivered.source.clone()),
            Slot::Requested | Slot::Failed(_) => None,
        }
    }

    /// The failure recorded for `request_url`, if its fetch failed.
    #[must_use]
    pub fn failure(&self, request_url: &str) -> Option<String> {
        match self.read().slots.get(request_url)? {
            Slot::Failed(message) => Some(message.clone()),
            Slot::Requested | Slot::Delivered(_) => None,
        }
    }

    /// Walk the static graph rooted at `entry_url` over what was delivered.
    ///
    /// An entry that was never delivered is itself the first fetch.
    ///
    /// # Errors
    /// The first failed fetch, unparsable module or unresolvable specifier
    /// the walk meets. The graph cannot run; the embedder reports it.
    pub fn graph_status(&self, entry_url: &str) -> Result<WebModuleGraph, WebModuleError> {
        self.walk(entry_url, true)
    }

    /// Whether the graph rooted at `entry_url` is complete, without asking
    /// for anything: a missing module is pending but stays unrequested, so
    /// the next [`Self::graph_status`] still reports it.
    ///
    /// # Errors
    /// As [`Self::graph_status`].
    pub fn peek_graph(&self, entry_url: &str) -> Result<WebModuleGraph, WebModuleError> {
        self.walk(entry_url, false)
    }

    /// How many times a delivered module source was parsed, by the walks
    /// that discover graphs and by the graphs that run them.
    #[must_use]
    pub fn parses(&self) -> u64 {
        self.read().parses
    }

    pub(crate) fn note_parse(&self) {
        self.write().parses += 1;
    }

    /// The walk's compile of the module delivered as `final_url` with
    /// exactly `text`, handed over once.
    pub(crate) fn take_compiled(&self, final_url: &str, text: &str) -> Option<PrecompiledModule> {
        let mut state = self.write();
        let delivered = match state.slots.get_mut(final_url) {
            Some(Slot::Delivered(delivered)) if delivered.source.final_url == final_url => {
                delivered
            }
            // A redirect keys the slot by the URL that was requested.
            _ => state.slots.values_mut().find_map(|slot| match slot {
                Slot::Delivered(delivered) if delivered.source.final_url == final_url => {
                    Some(delivered)
                }
                Slot::Requested | Slot::Delivered(_) | Slot::Failed(_) => None,
            })?,
        };
        if delivered.source.text != text {
            return None;
        }
        delivered.compiled.take()
    }

    fn walk(&self, entry_url: &str, request: bool) -> Result<WebModuleGraph, WebModuleError> {
        let mut guard = self.write();
        let state = &mut *guard;
        let mut fetch = Vec::new();
        let mut pending = false;
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack = vec![entry_url.to_owned()];
        while let Some(url) = stack.pop() {
            if crate::module_loader::is_data_url(&url) || !visited.insert(url.clone()) {
                continue;
            }
            let delivered = match state.slots.get_mut(&url) {
                None => {
                    if request {
                        state.slots.insert(url.clone(), Slot::Requested);
                        fetch.push(url);
                    }
                    pending = true;
                    continue;
                }
                Some(Slot::Requested) => {
                    pending = true;
                    continue;
                }
                Some(Slot::Failed(message)) => {
                    return Err(WebModuleError::Fetch {
                        url,
                        message: message.clone(),
                    });
                }
                Some(Slot::Delivered(delivered)) => delivered,
            };
            let final_url = delivered.source.final_url.clone();
            if delivered.requests.is_none()
                && let Some(bytes) = delivered.code_cache.take()
                && let Some((cached, kept)) = crate::module_graph::decode_web_module_cache(
                    &bytes,
                    &delivered.source.text,
                    &final_url,
                )
            {
                delivered.compiled = cached.compiled;
                delivered.requests = Some(Ok(cached.requests));
                state.kept.push((final_url.clone(), kept));
                state.kept_urls.push((final_url.clone(), url.clone()));
            }
            if delivered.requests.is_none() {
                state.parses += 1;
                let import_map = &state.import_map;
                let scan = scan_requests(&delivered.source, |specifier| {
                    let base = Url::parse(strip_inline_marker(&final_url)).ok()?;
                    import_map
                        .resolve(specifier, &base)
                        .ok()
                        .map(|url| url.to_string())
                });
                delivered.requests = Some(scan.map(|scan| {
                    delivered.compiled = scan.compiled;
                    scan.requests
                }));
                let delivered = &**delivered;
                produce_code_cache_parts(
                    state.produce_code_caches,
                    &mut state.fresh_code_caches,
                    &mut state.kept,
                    &mut state.kept_urls,
                    &url,
                    &final_url,
                    delivered,
                );
            }
            let requests =
                delivered
                    .requests
                    .clone()
                    .expect("scanned above")
                    .map_err(|message| WebModuleError::Parse {
                        url: final_url.clone(),
                        message,
                    })?;
            for request in requests.iter().filter(|request| !request.dynamic) {
                let target = Url::parse(strip_inline_marker(&final_url))
                    .map_err(|error| ImportMapResolveError {
                        message: format!("invalid module URL {final_url}: {error}"),
                    })
                    .and_then(|base| state.import_map.resolve(&request.specifier, &base))
                    .map_err(|error| WebModuleError::Resolve {
                        referrer: final_url.clone(),
                        message: error.message,
                    })?;
                stack.push(target.to_string());
            }
        }
        if pending {
            Ok(WebModuleGraph::Pending { fetch })
        } else {
            Ok(WebModuleGraph::Ready)
        }
    }
}

/// Encode a fresh compile of `delivered` for the embedder's code cache, and
/// keep its first-call bodies, when the map produces code caches.
fn produce_code_cache(
    state: &mut WebModuleState,
    request_url: &str,
    final_url: &str,
    delivered: &Delivered,
) {
    produce_code_cache_parts(
        state.produce_code_caches,
        &mut state.fresh_code_caches,
        &mut state.kept,
        &mut state.kept_urls,
        request_url,
        final_url,
        delivered,
    );
}

fn produce_code_cache_parts(
    produce: bool,
    fresh_code_caches: &mut Vec<WebModuleCodeCache>,
    kept: &mut Vec<(String, otter_vm::KeptScript)>,
    kept_urls: &mut Vec<(String, String)>,
    request_url: &str,
    final_url: &str,
    delivered: &Delivered,
) {
    if produce
        && let Some(compiled) = &delivered.compiled
        && let Some(bytes) = crate::module_graph::encode_web_module_cache(
            &delivered.source.text,
            final_url,
            compiled,
        )
    {
        fresh_code_caches.push(WebModuleCodeCache {
            request_url: request_url.to_owned(),
            bytes: crate::code_cache::with_bodies(bytes.clone(), &[]),
        });
        kept.push((
            final_url.to_owned(),
            otter_vm::KeptScript {
                top: bytes,
                bodies: Vec::new(),
            },
        ));
        kept_urls.push((final_url.to_owned(), request_url.to_owned()));
    }
}

fn scan_requests(
    source: &WebModuleSource,
    resolve: impl Fn(&str) -> Option<String>,
) -> Result<crate::module_graph::WebModuleScan, String> {
    let text = otter_resource::SharedSource::admit(
        &otter_resource::ResourceAccount::default(),
        source.text.clone(),
    )
    .map_err(|error| error.to_string())?;
    scan_web_module(&text, &source.final_url, resolve)
}

/// `url` without the inline-module marker fragment, if it carries one.
#[must_use]
pub fn strip_inline_marker(url: &str) -> &str {
    otter_vm::strip_inline_module_marker(url)
}
