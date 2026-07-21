//! Async remote-module provider scheduling, caching, and cancellation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use otter_runtime::embedding::{
    CapabilitySet, Otter, Permission, RemoteModuleError, RemoteModuleFuture, RemoteModuleProvider,
    RemoteModuleRequest, RemoteModuleSource, SourceInput,
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
                        match sources.get(&request.url).cloned() {
                            Some(source) => Ok(RemoteModuleSource {
                                source,
                                content_type: Some("text/javascript".to_string()),
                                final_url: final_urls.get(&request.url)
                                    .cloned()
                                    .unwrap_or_else(|| request.url.clone()),
                            }),
                            None => Err(RemoteModuleError::Fetch {
                                url: request.url.clone(),
                                message: "missing test module".to_string(),
                            }),
                        }
                    }
                }
            };
            state.active.fetch_sub(1, Ordering::AcqRel);
            result
        })
    }
}

fn allowed_net() -> CapabilitySet {
    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::AllowAll;
    capabilities
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
    let sources = Arc::new(BTreeMap::from([
        (
            "https://modules.test/alias-a.js".to_string(),
            "export const value = 42;".to_string(),
        ),
        (
            "https://modules.test/alias-b.js".to_string(),
            "export const value = 42;".to_string(),
        ),
    ]));
    let canonical = "https://cdn.modules.test/canonical.js".to_string();
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
    assert_eq!(state.calls.load(Ordering::Acquire), 2);
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
    otter.handle().shutdown();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !state.cancelled.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider observes disposal");
    let _ = tokio::time::timeout(Duration::from_secs(1), running)
        .await
        .expect("waiting task exits after disposal");
}
