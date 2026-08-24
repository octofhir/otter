//! Async remote-module provider scheduling, caching, and cancellation.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use otter_runtime::default_check_capability;
use otter_runtime::embedding::{
    CapabilityRequest, CapabilitySet, Otter, OtterError, Permission, RemoteModuleError,
    RemoteModuleFuture, RemoteModuleProvider, RemoteModuleRequest, RemoteModuleResponse,
    RuntimeCapability, SourceInput,
};

#[derive(Debug, Default)]
struct ProviderState {
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    cancelled: AtomicBool,
    started: tokio::sync::Notify,
}

#[derive(Debug)]
struct TestProvider {
    sources: Arc<BTreeMap<String, String>>,
    final_urls: Arc<BTreeMap<String, String>>,
    state: Arc<ProviderState>,
    delay: Duration,
    wait_for_cancel: bool,
}

impl RemoteModuleProvider for TestProvider {
    fn fetch(&self, request: RemoteModuleRequest) -> RemoteModuleFuture {
        let sources = self.sources.clone();
        let final_urls = self.final_urls.clone();
        let state = self.state.clone();
        let delay = self.delay;
        let wait_for_cancel = self.wait_for_cancel;
        Box::pin(async move {
            state.calls.fetch_add(1, Ordering::Relaxed);
            let active = state.active.fetch_add(1, Ordering::AcqRel) + 1;
            state.max_active.fetch_max(active, Ordering::AcqRel);
            state.started.notify_waiters();
            let result = if wait_for_cancel {
                request.cancellation.cancelled().await;
                state.cancelled.store(true, Ordering::Release);
                Err(RemoteModuleError::Cancelled)
            } else {
                tokio::select! {
                    () = request.cancellation.cancelled() => {
                        state.cancelled.store(true, Ordering::Release);
                        Err(RemoteModuleError::Cancelled)
                    }
                    () = tokio::time::sleep(delay) => {
                        if let Some(location) = final_urls.get(&request.url).cloned() {
                            Ok(RemoteModuleResponse::Redirect { location })
                        } else {
                            match sources.get(&request.url).cloned() {
                            Some(source) => Ok(RemoteModuleResponse::Source {
                                source: otter_runtime::SharedSource::admit(
                                    &request.account,
                                    source,
                                )
                                .expect("test source admission"),
                                content_type: Some("text/javascript".to_string()),
                            }),
                            None => Err(RemoteModuleError::Fetch {
                                url: request.url.clone(),
                                message: "missing test module".to_string(),
                            }),
                            }
                        }
                    }
                }
            };
            state.active.fetch_sub(1, Ordering::AcqRel);
            result
        })
    }
}

#[derive(Debug, Default)]
struct CancellationDefyingState {
    calls: AtomicUsize,
    returned_after_cancel: AtomicBool,
}

#[derive(Debug)]
struct CancellationDefyingProvider {
    state: Arc<CancellationDefyingState>,
}

impl RemoteModuleProvider for CancellationDefyingProvider {
    fn fetch(&self, request: RemoteModuleRequest) -> RemoteModuleFuture {
        let state = self.state.clone();
        Box::pin(async move {
            let call = state.calls.fetch_add(1, Ordering::AcqRel);
            if call == 0 {
                request.cancellation.cancelled().await;
                state.returned_after_cancel.store(true, Ordering::Release);
                return Ok(RemoteModuleResponse::Source {
                    source: otter_runtime::SharedSource::admit(
                        &request.account,
                        "export const late = 1;".to_string(),
                    )
                    .expect("test source admission"),
                    content_type: Some("text/javascript".to_string()),
                });
            }
            Err(RemoteModuleError::Fetch {
                url: request.url,
                message: "late cancelled response must not populate the cache".to_string(),
            })
        })
    }
}

fn allowed_net() -> CapabilitySet {
    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::AllowAll;
    capabilities
}

fn spawn_module_redirect(location: String) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind redirect endpoint");
    let address = listener.local_addr().expect("redirect addr");
    let thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept module request");
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request);
        let response =
            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n");
        stream
            .write_all(response.as_bytes())
            .expect("write module redirect");
    });
    (format!("http://{address}/module.js"), thread)
}

fn spawn_module_source(source: String) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind module endpoint");
    let address = listener.local_addr().expect("module addr");
    let thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept module request");
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\n\r\n{source}",
            source.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write module source");
    });
    (format!("http://{address}/module.js"), thread)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_provider_authorizes_redirect_before_connection() {
    let denied_listener = TcpListener::bind("127.0.0.1:0").expect("bind denied endpoint");
    denied_listener
        .set_nonblocking(true)
        .expect("nonblocking denied endpoint");
    let denied_url = format!(
        "http://{}/denied.js",
        denied_listener.local_addr().expect("denied addr")
    );
    let (initial_url, redirect_server) = spawn_module_redirect(denied_url.clone());
    let parsed_initial = url::Url::parse(&initial_url).expect("initial URL");
    let initial_authority = format!(
        "{}:{}",
        parsed_initial.host_str().expect("initial host"),
        parsed_initial.port().expect("initial port")
    );
    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::allow([initial_authority]);
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hook_calls = calls.clone();
    let otter = Otter::builder()
        .capabilities(capabilities)
        .capability_hook(
            move |capabilities: &CapabilitySet,
                  capability: RuntimeCapability,
                  request: &CapabilityRequest<'_>| {
                if let CapabilityRequest::Network { url, initiator } = request {
                    hook_calls
                        .lock()
                        .expect("hook calls")
                        .push((url.to_string(), initiator.map(url::Url::to_string)));
                }
                default_check_capability(capabilities, capability, request)
            },
        )
        .build()
        .expect("otter");
    let entry_url = "https://entry.test/main.js";
    let error = otter
        .run_module_source(
            SourceInput::from_javascript(format!("import {initial_url:?};")),
            entry_url,
        )
        .await
        .expect_err("redirect target must be denied");
    let OtterError::Compile { diagnostics } = error else {
        panic!("unexpected denial: {error:?}");
    };
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "MODULE_CAPABILITY_DENIED"),
        "unexpected diagnostics: {diagnostics:?}"
    );
    redirect_server.join().expect("redirect server thread");
    assert_eq!(
        calls.lock().expect("hook calls").as_slice(),
        &[
            (initial_url.clone(), Some(entry_url.to_string())),
            (denied_url.clone(), Some(initial_url)),
        ]
    );
    match denied_listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("denied module redirect endpoint observed a connection"),
        Err(error) => panic!("denied endpoint accept failed: {error}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_provider_follows_allowed_redirect_with_final_identity() {
    let (final_url, source_server) = spawn_module_source(
        "globalThis.redirectModuleUrl = import.meta.url; export const value = 42;".to_string(),
    );
    let (initial_url, redirect_server) = spawn_module_redirect(final_url.clone());
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hook_calls = calls.clone();
    let otter = Otter::builder()
        .capabilities(allowed_net())
        .capability_hook(
            move |capabilities: &CapabilitySet,
                  capability: RuntimeCapability,
                  request: &CapabilityRequest<'_>| {
                if let CapabilityRequest::Network { url, initiator } = request {
                    hook_calls
                        .lock()
                        .expect("hook calls")
                        .push((url.to_string(), initiator.map(url::Url::to_string)));
                }
                default_check_capability(capabilities, capability, request)
            },
        )
        .build()
        .expect("otter");
    let entry_url = "https://entry.test/allowed.js";
    otter
        .run_module_source(
            SourceInput::from_javascript(format!("import {initial_url:?};")),
            entry_url,
        )
        .await
        .expect("allowed redirect module");
    redirect_server.join().expect("redirect server thread");
    source_server.join().expect("source server thread");
    assert_eq!(
        otter
            .handle()
            .eval(SourceInput::from_javascript("redirectModuleUrl"))
            .await
            .expect("read redirected module identity")
            .completion_string(),
        final_url
    );
    assert_eq!(
        calls.lock().expect("hook calls").as_slice(),
        &[
            (initial_url.clone(), Some(entry_url.to_string())),
            (final_url, Some(initial_url.clone())),
            (initial_url, Some(entry_url.to_string())),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn remote_fetches_are_parallel_bounded_and_cached_across_commands() {
    let state = Arc::new(ProviderState::default());
    let sources = Arc::new(
        (0..12)
            .map(|index| {
                (
                    format!("https://modules.test/dep-{index}.js"),
                    format!("export const value{index} = {index};"),
                )
            })
            .collect(),
    );
    let imports = (0..12)
        .map(|index| format!("import {{ value{index} }} from './dep-{index}.js';"))
        .collect::<Vec<_>>()
        .join("\n");
    let otter = Otter::builder()
        .capabilities(allowed_net())
        .remote_module_provider(TestProvider {
            sources,
            final_urls: Arc::new(BTreeMap::new()),
            state: state.clone(),
            delay: Duration::from_millis(20),
            wait_for_cancel: false,
        })
        .build()
        .expect("otter");

    for suffix in ["first", "second"] {
        otter
            .run_module_source(
                SourceInput::from_javascript(format!(
                    "{imports}\nglobalThis.{suffix} = value0 + value11;"
                )),
                format!("https://modules.test/{suffix}.js"),
            )
            .await
            .expect("remote graph");
    }

    assert_eq!(state.calls.load(Ordering::Acquire), 12);
    let max_active = state.max_active.load(Ordering::Acquire);
    assert!(max_active > 1, "provider requests must overlap");
    assert!(max_active <= 8, "graph concurrency bound must hold");
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_redirect_urls_share_one_canonical_module_identity() {
    let state = Arc::new(ProviderState::default());
    let canonical = "https://cdn.modules.test/canonical.js".to_string();
    let sources = Arc::new(BTreeMap::from([(
        canonical.clone(),
        "export const value = 42;".to_string(),
    )]));
    let final_urls = Arc::new(BTreeMap::from([
        (
            "https://modules.test/alias-a.js".to_string(),
            canonical.clone(),
        ),
        ("https://modules.test/alias-b.js".to_string(), canonical),
    ]));
    let otter = Otter::builder()
        .capabilities(allowed_net())
        .remote_module_provider(TestProvider {
            sources,
            final_urls,
            state: state.clone(),
            delay: Duration::ZERO,
            wait_for_cancel: false,
        })
        .build()
        .expect("otter");

    otter
        .run_module_source(
            SourceInput::from_javascript(
                "import { value as a } from './alias-a.js';\n\
                 import { value as b } from './alias-b.js';\n\
                 export const answer = a + b;",
            ),
            "https://modules.test/redirect-entry.js",
        )
        .await
        .expect("redirect aliases share the canonical module record");

    otter
        .run_module_source(
            SourceInput::from_javascript(
                "import { value } from 'https://cdn.modules.test/canonical.js';\n\
                 export const answer = value;",
            ),
            "https://modules.test/canonical-cache-entry.js",
        )
        .await
        .expect("post-redirect canonical URL is reusable from cache");
    // Two aliases each produce one explicit redirect hop. Their concurrent
    // chains may both request the canonical source before either publishes it;
    // the later command must reuse the canonical cache entry.
    assert_eq!(state.calls.load(Ordering::Acquire), 4);
}

#[tokio::test(flavor = "current_thread")]
async fn cached_redirect_alias_rechecks_its_final_capability() {
    let state = Arc::new(ProviderState::default());
    let alias = "https://modules.test/policy-alias.js".to_string();
    let canonical = "https://cdn.modules.test/policy-final.js".to_string();
    let deny_final = Arc::new(AtomicBool::new(false));
    let hook_deny_final = deny_final.clone();
    let hook_canonical = canonical.clone();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hook_calls = calls.clone();
    let otter = Otter::builder()
        .capabilities(allowed_net())
        .capability_hook(
            move |capabilities: &CapabilitySet,
                  capability: RuntimeCapability,
                  request: &CapabilityRequest<'_>| {
                if let CapabilityRequest::Network { url, initiator } = request {
                    hook_calls
                        .lock()
                        .expect("hook calls")
                        .push((url.to_string(), initiator.map(url::Url::to_string)));
                    if hook_deny_final.load(Ordering::Acquire) && url.as_str() == hook_canonical {
                        return false;
                    }
                }
                default_check_capability(capabilities, capability, request)
            },
        )
        .remote_module_provider(TestProvider {
            sources: Arc::new(BTreeMap::from([(
                canonical.clone(),
                "export const value = 42;".to_string(),
            )])),
            final_urls: Arc::new(BTreeMap::from([(alias.clone(), canonical.clone())])),
            state: state.clone(),
            delay: Duration::ZERO,
            wait_for_cancel: false,
        })
        .build()
        .expect("otter");

    otter
        .run_module_source(
            SourceInput::from_javascript(format!("import {alias:?};")),
            "https://entry.test/cache-first.js",
        )
        .await
        .expect("first alias load is permitted");
    deny_final.store(true, Ordering::Release);
    let error = otter
        .run_module_source(
            SourceInput::from_javascript(format!("import {alias:?};")),
            "https://entry.test/cache-second.js",
        )
        .await
        .expect_err("cached alias final URL must be re-authorized");
    let OtterError::Compile { diagnostics } = error else {
        panic!("unexpected cached-alias denial: {error:?}");
    };
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "MODULE_CAPABILITY_DENIED"),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert_eq!(
        state.calls.load(Ordering::Acquire),
        2,
        "the denied cached final URL must not call the provider again"
    );
    assert_eq!(
        calls.lock().expect("hook calls").last(),
        Some(&(canonical, Some(alias)))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn provider_redirect_loop_is_bounded_by_the_runtime() {
    let state = Arc::new(ProviderState::default());
    let first = "https://modules.test/loop-a.js".to_string();
    let second = "https://modules.test/loop-b.js".to_string();
    let otter = Otter::builder()
        .capabilities(allowed_net())
        .remote_module_provider(TestProvider {
            sources: Arc::new(BTreeMap::new()),
            final_urls: Arc::new(BTreeMap::from([
                (first.clone(), second.clone()),
                (second, first.clone()),
            ])),
            state: state.clone(),
            delay: Duration::ZERO,
            wait_for_cancel: false,
        })
        .build()
        .expect("otter");

    let error = otter
        .run_module_source(
            SourceInput::from_javascript(format!("import {first:?};")),
            "https://modules.test/loop-entry.js",
        )
        .await
        .expect_err("redirect loop must fail");
    assert_module_redirect_error(error);
    assert_eq!(state.calls.load(Ordering::Acquire), 11);
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_provider_redirect_is_a_typed_load_error() {
    let state = Arc::new(ProviderState::default());
    let target = "https://modules.test/malformed.js".to_string();
    let otter = Otter::builder()
        .capabilities(allowed_net())
        .remote_module_provider(TestProvider {
            sources: Arc::new(BTreeMap::new()),
            final_urls: Arc::new(BTreeMap::from([(target.clone(), "http://[".to_string())])),
            state: state.clone(),
            delay: Duration::ZERO,
            wait_for_cancel: false,
        })
        .build()
        .expect("otter");

    let error = otter
        .run_module_source(
            SourceInput::from_javascript(format!("import {target:?};")),
            "https://modules.test/malformed-entry.js",
        )
        .await
        .expect_err("malformed redirect must fail");
    assert_module_redirect_error(error);
    assert_eq!(state.calls.load(Ordering::Acquire), 1);
}

fn assert_module_redirect_error(error: OtterError) {
    let OtterError::Compile { diagnostics } = error else {
        panic!("expected typed module load error, got {error:?}");
    };
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "MODULE_RESOLUTION_ERROR"),
        "unexpected diagnostics: {diagnostics:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn command_timeout_cancels_remote_provider() {
    let state = Arc::new(ProviderState::default());
    let otter = Otter::builder()
        .capabilities(allowed_net())
        .timeout(Duration::from_millis(30))
        .remote_module_provider(TestProvider {
            sources: Arc::new(BTreeMap::new()),
            final_urls: Arc::new(BTreeMap::new()),
            state: state.clone(),
            delay: Duration::ZERO,
            wait_for_cancel: true,
        })
        .build()
        .expect("otter");

    let error = otter
        .run_module_source(
            SourceInput::from_javascript("import './slow.js';"),
            "https://modules.test/timeout.js",
        )
        .await
        .expect_err("preparation must time out");
    assert!(matches!(error, otter_runtime::OtterError::Timeout { .. }));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !state.cancelled.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider observes cancellation");
}

#[tokio::test(flavor = "current_thread")]
async fn provider_source_returned_after_cancellation_is_not_cached() {
    let state = Arc::new(CancellationDefyingState::default());
    let otter = Otter::builder()
        .capabilities(allowed_net())
        .timeout(Duration::from_millis(30))
        .remote_module_provider(CancellationDefyingProvider {
            state: state.clone(),
        })
        .build()
        .expect("otter");
    let target = "https://modules.test/late.js";

    let error = otter
        .run_module_source(
            SourceInput::from_javascript(format!("import {target:?};")),
            "https://modules.test/late-first.js",
        )
        .await
        .expect_err("first request times out");
    assert!(matches!(error, OtterError::Timeout { .. }));
    assert!(
        state.returned_after_cancel.load(Ordering::Acquire),
        "test provider must deliberately return Source after cancellation"
    );

    let error = otter
        .run_module_source(
            SourceInput::from_javascript(format!("import {target:?};")),
            "https://modules.test/late-second.js",
        )
        .await
        .expect_err("late cancelled Source must not become a cache hit");
    let OtterError::Compile { diagnostics } = error else {
        panic!("expected the second provider error, got {error:?}");
    };
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "MODULE_RESOLUTION_ERROR"),
        "unexpected diagnostics: {diagnostics:?}"
    );
    assert_eq!(state.calls.load(Ordering::Acquire), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_shutdown_cancels_inflight_remote_provider() {
    let state = Arc::new(ProviderState::default());
    let otter = Otter::builder()
        .capabilities(allowed_net())
        .timeout(Duration::ZERO)
        .remote_module_provider(TestProvider {
            sources: Arc::new(BTreeMap::new()),
            final_urls: Arc::new(BTreeMap::new()),
            state: state.clone(),
            delay: Duration::ZERO,
            wait_for_cancel: true,
        })
        .build()
        .expect("otter");
    let running = {
        let otter = otter.clone();
        tokio::spawn(async move {
            otter
                .run_module_source(
                    SourceInput::from_javascript("import './slow.js';"),
                    "https://modules.test/dispose.js",
                )
                .await
        })
    };
    while state.calls.load(Ordering::Acquire) == 0 {
        state.started.notified().await;
    }
    tokio::time::timeout(Duration::from_secs(1), otter.handle().shutdown_and_wait())
        .await
        .expect("current-thread runtime stays responsive during disposal");
    assert!(state.cancelled.load(Ordering::Acquire));
    let _ = tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .expect("waiting task exits after disposal");
}
