//! Slice C regression coverage: dynamic `import()` routing
//! through the unified loader and capability gating for
//! `http:` / `https:` specifiers.
//!
//! ENGINE_REFACTOR_EXECUTION_PLAN §P2.1 acceptance:
//! "Dynamic-import test denies a registry/HTTP specifier when
//! the matching capability is absent and resolves it when
//! present." This file pins both halves:
//!
//! 1. Without `Capability::Net`, an HTTPS import (static or
//!    dynamic literal) surfaces `MODULE_CAPABILITY_DENIED`
//!    before any network I/O.
//! 2. With `Capability::Net` granted, the Layer B loader fetches
//!    HTTP module graphs and prepares them off-isolate before
//!    realm-local evaluation. Direct Layer A runtimes without a
//!    configured host fetcher still report a resolution error
//!    after the capability gate passes.
//! 3. Slow remote graph preparation does not prevent a concurrent
//!    command from completing on the page isolate.
//!
//! Local literal `import("./mod.ts")` continues to work
//! through the pre-resolved module graph; that path is also
//! pinned here so the capability machinery does not
//! accidentally gate file:// imports.
//!
//! Spec: <https://tc39.es/ecma262/#sec-import-call-runtime-semantics-evaluation>
//!       <https://tc39.es/ecma262/#sec-HostLoadImportedModule>

use std::path::Path;

use otter_runtime::{CapabilitySet, Otter, OtterError, Permission, RuntimeBuilder};

fn run_with_capabilities(entry: &Path, capabilities: CapabilitySet) -> Result<(), OtterError> {
    let mut runtime = RuntimeBuilder::default()
        .capabilities(capabilities)
        .build()?;
    runtime.run_module(entry).map(|_| ())
}

async fn run_module_async(entry: &Path) -> Result<(), OtterError> {
    let otter = Otter::builder()
        .capabilities(CapabilitySet::allow_all())
        .build()
        .expect("otter");
    otter.run_module(entry).await.map(|_| ())
}

/// Spawn a one-shot HTTP responder on a free local port. Returns
/// the bound `127.0.0.1:port` address. The server accepts a single
/// connection, sends `body` as `text/javascript`, and exits.
async fn spawn_one_shot_http_server(body: String) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        // Accept connections until the spawning test drops the task.
        // The runtime drives at most one fetch per HTTPS dynamic-
        // import call, but a repeat-import test issues two GETs
        // (cache hit avoids the second one in production; this is
        // belt-and-braces for diagnostics).
        loop {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
        }
    });
    addr
}

async fn spawn_module_graph_http_server() -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 2048];
            let read = stream.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]);
            let body = if request.starts_with("GET /dep.js ") {
                "export const answer = 42;"
            } else {
                "import { answer } from './dep.js'; export { answer };"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    addr
}

/// HTTPS specifier without `Net` capability surfaces
/// `MODULE_CAPABILITY_DENIED` before any network I/O.
#[test]
fn https_static_import_denied_without_net_capability() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.ts");
    std::fs::write(
        &entry,
        r#"import { x } from "https://example.invalid/mod.js"; console.log(x);"#,
    )
    .unwrap();

    let err = run_with_capabilities(&entry, CapabilitySet::sandbox()).expect_err("must deny");
    match err {
        OtterError::Compile { diagnostics } => {
            let diag = diagnostics
                .iter()
                .find(|d| d.code == "MODULE_CAPABILITY_DENIED")
                .unwrap_or_else(|| {
                    panic!("expected MODULE_CAPABILITY_DENIED, got {diagnostics:?}")
                });
            assert!(
                diag.message.contains("https://example.invalid/mod.js")
                    && diag.message.contains("net"),
                "unexpected message: {}",
                diag.message
            );
        }
        other => panic!("expected Compile error, got {other:?}"),
    }
}

/// Dynamic literal `import("https://...")` is pre-resolved at
/// graph load time, so the capability check fires there too.
#[test]
fn https_dynamic_literal_import_denied_without_net_capability() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.ts");
    std::fs::write(
        &entry,
        r#"
            import("https://example.invalid/mod.js").then(
              (m) => { void m; },
              (e) => { void e; },
            );
        "#,
    )
    .unwrap();

    let err = run_with_capabilities(&entry, CapabilitySet::sandbox()).expect_err("must deny");
    match err {
        OtterError::Compile { diagnostics } => {
            assert!(
                diagnostics
                    .iter()
                    .any(|d| d.code == "MODULE_CAPABILITY_DENIED"),
                "expected MODULE_CAPABILITY_DENIED, got {diagnostics:?}"
            );
        }
        other => panic!("expected Compile error, got {other:?}"),
    }
}

/// With `Net` granted for the host, the capability check passes.
/// The HTTPS fetcher itself is a separate slice; the loader
/// surfaces `MODULE_RESOLUTION_ERROR` (not `MODULE_CAPABILITY_DENIED`)
/// once the gating boundary is satisfied.
#[test]
fn https_import_passes_capability_gate_when_net_allowed_for_host() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.ts");
    std::fs::write(
        &entry,
        r#"import { x } from "https://example.invalid/mod.js"; console.log(x);"#,
    )
    .unwrap();

    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::allow(vec!["example.invalid".to_string()]);
    let err = run_with_capabilities(&entry, capabilities).expect_err("fetcher missing");
    match err {
        OtterError::Compile { diagnostics } => {
            assert!(
                diagnostics
                    .iter()
                    .all(|d| d.code != "MODULE_CAPABILITY_DENIED"),
                "capability gate should have passed, got {diagnostics:?}"
            );
            assert!(
                diagnostics
                    .iter()
                    .any(|d| d.code == "MODULE_RESOLUTION_ERROR"),
                "expected MODULE_RESOLUTION_ERROR after capability passed, got {diagnostics:?}"
            );
        }
        other => panic!("expected Compile error, got {other:?}"),
    }
}

/// `Permission::AllowAll` (e.g. `--allow-all`) bypasses the
/// per-host check.
#[test]
fn https_import_passes_capability_gate_under_allow_all() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.ts");
    std::fs::write(
        &entry,
        r#"import { x } from "https://example.invalid/mod.js"; console.log(x);"#,
    )
    .unwrap();

    let err =
        run_with_capabilities(&entry, CapabilitySet::allow_all()).expect_err("fetcher missing");
    match err {
        OtterError::Compile { diagnostics } => {
            assert!(
                diagnostics
                    .iter()
                    .all(|d| d.code != "MODULE_CAPABILITY_DENIED"),
                "AllowAll must bypass capability gating, got {diagnostics:?}"
            );
        }
        other => panic!("expected Compile error, got {other:?}"),
    }
}

#[test]
fn module_capability_hook_receives_target_and_initiator_urls() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.ts");
    std::fs::write(&entry, r#"import "https://example.invalid/path/mod.js";"#).unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let hook_seen = seen.clone();
    let mut runtime = RuntimeBuilder::default()
        .capabilities(CapabilitySet::sandbox())
        .capability_hook(
            move |_capabilities: &CapabilitySet,
                  capability: otter_runtime::RuntimeCapability,
                  request: &otter_runtime::CapabilityRequest<'_>| {
                let otter_runtime::CapabilityRequest::Network { url, initiator } = request else {
                    return false;
                };
                if capability != otter_runtime::RuntimeCapability::Net {
                    return false;
                }
                *hook_seen.lock().expect("seen") =
                    Some((url.to_string(), initiator.map(url::Url::to_string)));
                true
            },
        )
        .build()
        .expect("runtime");

    let error = runtime
        .run_module(&entry)
        .expect_err("direct runtime has no remote fetch provider");
    assert!(matches!(error, OtterError::Compile { .. }));
    let (target, initiator) = seen.lock().expect("seen").clone().expect("hook called");
    assert_eq!(target, "https://example.invalid/path/mod.js");
    let canonical_entry = std::fs::canonicalize(entry).expect("canonical entry");
    let expected_initiator = url::Url::from_file_path(canonical_entry).expect("entry URL");
    assert_eq!(initiator.as_deref(), Some(expected_initiator.as_str()));
}

/// Local `file://` imports must not be gated by `Net` — the
/// capability machinery only fires for the `http:` / `https:`
/// shapes today.
#[test]
fn local_file_import_unaffected_by_net_capability() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("dep.ts"), "export const x = 42;\n").unwrap();
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            import { x } from "./dep.ts";
            if (x !== 42) throw new Error("local import broken: " + x);
        "#,
    )
    .unwrap();

    run_with_capabilities(&dir.path().join("entry.ts"), CapabilitySet::allow_all())
        .expect("local import must run under allow_all");
}

/// Top-level `await import("./literal")` must settle through the
/// pre-resolved module graph and observe the dependency's
/// exports. Smoke test for the pre-Slice-A audit's third
/// foundation gap (which Slice A's linker fix incidentally
/// closed).
#[test]
fn dynamic_literal_import_settles_top_level_await() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("dep.ts"),
        "export const greeting = \"hello\";\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            const mod = await import("./dep.ts");
            if (mod.greeting !== "hello") {
                throw new Error("dynamic import settle broken: " + mod.greeting);
            }
        "#,
    )
    .unwrap();

    run_with_capabilities(&dir.path().join("entry.ts"), CapabilitySet::allow_all())
        .expect("dynamic literal import must settle");
}

/// Non-literal `import(specifierVariable)` must compile and
/// resolve through the same pre-linked module graph. The specifier
/// expression evaluates at runtime; the linker has already merged
/// the target module because it was imported statically (or as a
/// literal `import("./x")`) earlier in the program. Pinned by
/// ENGINE_REFACTOR_EXECUTION_PLAN §P2.2 Slice C follow-up.
#[test]
fn dynamic_import_with_variable_specifier_resolves_through_linker_graph() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("dep.ts"),
        "export const greeting = \"hello-from-var\";\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            // Static import keeps the module in the pre-linked graph
            // so the linker has populated `module_resolutions` for it.
            import "./dep.ts";
            const spec = "./dep.ts";
            const mod = await import(spec);
            if (mod.greeting !== "hello-from-var") {
                throw new Error("dynamic var import broken: " + mod.greeting);
            }
        "#,
    )
    .unwrap();

    run_with_capabilities(&dir.path().join("entry.ts"), CapabilitySet::allow_all())
        .expect("dynamic var import must settle");
}

/// Non-literal `import(specifierVariable)` where the specifier
/// does not match any pre-linked module rejects with a TypeError
/// (catchable via `.catch`) rather than aborting the script with
/// `unknown intrinsic method`. The current slice does not yet
/// load brand-new modules on demand — that is a follow-up — but
/// the rejection path must already be observable to JS code.
///
/// The specifier is built at runtime so the linker does not try
/// to resolve it during graph load (a literal would surface as
/// `MODULE_RESOLUTION_ERROR` at compile time before the dynamic
/// import opcode ever runs).
#[test]
fn dynamic_import_with_unknown_specifier_rejects_with_typeerror() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            let caught = null;
            const spec = "./" + "does-not-exist.ts";
            try {
                await import(spec);
            } catch (e) {
                caught = e;
            }
            if (caught === null) {
                throw new Error("expected dynamic import rejection");
            }
            if (!(caught instanceof TypeError)) {
                throw new Error("expected TypeError, got " + caught);
            }
            if (!caught.message.includes("does-not-exist.ts")) {
                throw new Error("rejection missing specifier: " + caught.message);
            }
        "#,
    )
    .unwrap();

    run_with_capabilities(&dir.path().join("entry.ts"), CapabilitySet::allow_all())
        .expect("rejected dynamic import must be catchable");
}

/// On-demand load: `await import(spec)` where `spec` points at a
/// module file the linker has not seen statically. The runtime
/// dynamic-import scheduler resolves the specifier, loads +
/// compiles + links + evaluates the new module, and settles the
/// promise with its namespace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_import_loads_new_module_on_demand() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("on_demand.ts"),
        "export const greeting = \"on-demand\";\nexport const metaUrl = import.meta.url;\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            // No static import — `on_demand.ts` is invisible to the
            // linker. The dynamic-import scheduler loads it on the
            // inbox hop.
            const path = "./" + "on_demand.ts";
            const mod = await import(path);
            if (mod.greeting !== "on-demand") {
                throw new Error("bad namespace: " + mod.greeting);
            }
            if (!mod.metaUrl.endsWith("/on_demand.ts")) {
                throw new Error("bad dynamic import.meta.url: " + mod.metaUrl);
            }
        "#,
    )
    .unwrap();

    run_module_async(&dir.path().join("entry.ts"))
        .await
        .expect("on-demand import must load + settle");
}

/// On-demand load: target module's own static imports also load
/// transitively. `entry.ts` dynamically imports `outer.ts`, which
/// statically imports `inner.ts` — both must compile + evaluate
/// before the awaiting frame resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_import_loads_target_dependencies_transitively() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("inner.ts"),
        "export const inner_value = 7;\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("outer.ts"),
        "import { inner_value } from \"./inner.ts\";\nexport const outer_value = inner_value * 6;\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            const path = "./" + "outer.ts";
            const mod = await import(path);
            if (mod.outer_value !== 42) {
                throw new Error("transitive load broken: " + mod.outer_value);
            }
        "#,
    )
    .unwrap();

    run_module_async(&dir.path().join("entry.ts"))
        .await
        .expect("transitive on-demand load must complete");
}

/// On-demand load: the dynamically-loaded module's
/// `<module-init>` throws. §16.2.1.7 step 7.b.i requires the
/// rejection reason to be the original abrupt-completion value
/// — an `Error` instance with the spec-correct message — not a
/// stringified host diagnostic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_import_forwards_original_throw_value_from_module_init() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("boom.ts"),
        "throw new Error(\"init-boom\");\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            const path = "./" + "boom.ts";
            let caught = null;
            try {
                await import(path);
            } catch (e) {
                caught = e;
            }
            if (!(caught instanceof Error)) {
                throw new Error("expected Error instance, got " + caught);
            }
            if (caught.message !== "init-boom") {
                throw new Error("lost original throw payload: " + caught.message);
            }
        "#,
    )
    .unwrap();

    run_module_async(&dir.path().join("entry.ts"))
        .await
        .expect("init throw must forward as catchable Error instance");
}

/// On-demand load: `await import("http://127.0.0.1:PORT/...")`
/// fetches the source over HTTP, compiles, evaluates, and settles
/// with the namespace. The capability gate must pass for the host;
/// without it the loader rejects with `MODULE_CAPABILITY_DENIED`
/// (covered by sibling tests).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_import_fetches_https_module_with_net_capability() {
    let body =
        "export const greeting = \"http-served\";\nexport const metaUrl = import.meta.url;\n";
    let addr = spawn_one_shot_http_server(body.to_string()).await;
    let host = format!("{}", addr);
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.ts");
    std::fs::write(
        &entry,
        format!(
            r#"
            const url = "http://{host}/mod.js";
            const mod = await import(url);
            if (mod.greeting !== "http-served") {{
                throw new Error("bad HTTP namespace: " + mod.greeting);
            }}
            if (mod.metaUrl !== url) {{
                throw new Error("bad HTTP import.meta.url: " + mod.metaUrl);
            }}
            "#
        ),
    )
    .unwrap();

    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::allow(vec![host.clone()]);
    let otter = Otter::builder()
        .capabilities(capabilities)
        .build()
        .expect("otter");
    otter
        .run_module(entry)
        .await
        .expect("HTTPS dynamic import must fetch + evaluate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_http_import_settles_in_its_origin_realm() {
    let addr = spawn_one_shot_http_server(
        "export const answer = 42; export const url = import.meta.url;".to_string(),
    )
    .await;
    let host = addr.to_string();
    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::allow(vec![host.clone()]);
    let otter = Otter::builder()
        .capabilities(capabilities)
        .build()
        .expect("otter");
    let realm = otter.create_realm().await.expect("realm");
    let target = format!("http://{host}/dynamic.js");

    otter
        .run_module_source_in_realm(
            realm,
            otter_runtime::SourceInput::from_javascript(format!(
                "const target = {target:?}; const loaded = await import(target); globalThis.dynamicRealmResult = loaded.answer + ':' + loaded.url;"
            )),
            "https://origin.test/entry.js",
        )
        .await
        .expect("realm dynamic import");
    let result = otter
        .run_script_in_realm(
            realm,
            otter_runtime::SourceInput::from_javascript("dynamicRealmResult"),
            "realm:dynamic-result",
        )
        .await
        .expect("realm result");
    assert_eq!(result.completion_string(), format!("42:{target}"));
    let default = otter
        .run_script("typeof dynamicRealmResult")
        .await
        .expect("default realm check");
    assert_eq!(default.completion_string(), "undefined");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_http_import_prepares_static_dependencies_off_isolate() {
    let addr = spawn_module_graph_http_server().await;
    let host = addr.to_string();
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.ts");
    std::fs::write(
        &entry,
        format!(
            r#"
            const url = "http://{host}/mod.js";
            const mod = await import(url);
            if (mod.answer !== 42) {{
                throw new Error("bad transitive HTTP namespace: " + mod.answer);
            }}
            "#
        ),
    )
    .unwrap();

    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::allow(vec![host]);
    let otter = Otter::builder()
        .capabilities(capabilities)
        .build()
        .expect("otter");
    otter
        .run_module(entry)
        .await
        .expect("remote dynamic graph must load transitively");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_source_prepares_remote_graph_before_isolate_execution() {
    let addr = spawn_module_graph_http_server().await;
    let host = addr.to_string();
    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::allow(vec![host.clone()]);
    let otter = Otter::builder()
        .capabilities(capabilities)
        .build()
        .expect("otter");

    otter
        .run_module_source(
            otter_runtime::SourceInput::from_javascript(
                "import { answer } from './dep.js'; if (answer !== 42) throw new Error('bad answer');",
            ),
            format!("http://{host}/entry.js"),
        )
        .await
        .expect("in-memory remote entry must prepare dependencies off-isolate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_module_preparation_does_not_block_the_isolate() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (requested_tx, requested_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request).await;
        let _ = requested_tx.send(());
        let _ = release_rx.await;
        let body = "export const answer = 42;";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    });

    let host = addr.to_string();
    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::allow(vec![host.clone()]);
    let otter = Otter::builder()
        .capabilities(capabilities)
        .build()
        .expect("otter");
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.js");
    std::fs::write(
        &entry,
        format!(
            "import {{ answer }} from 'http://{host}/dep.js'; if (answer !== 42) throw new Error('bad answer');"
        ),
    )
    .expect("entry");
    let preparing = {
        let otter = otter.clone();
        tokio::spawn(async move { otter.run_module(entry).await })
    };

    requested_rx.await.expect("dependency request");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        otter.run_script("globalThis.stillResponsive = true;"),
    )
    .await
    .expect("isolate command must not wait for module I/O")
    .expect("responsive script");

    release_tx.send(()).expect("release response");
    preparing
        .await
        .expect("preparation task")
        .expect("prepared module executes");
}

/// HTTPS dynamic import must still respect the Net capability —
/// without it, the resolve-side capability check rejects before
/// any host I/O.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_import_https_without_net_capability_rejects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = dir.path().join("entry.ts");
    std::fs::write(
        &entry,
        r#"
            const url = "http://example.invalid/mod.js";
            let caught = null;
            try {
                await import(url);
            } catch (e) {
                caught = e;
            }
            if (!(caught instanceof TypeError)) {
                throw new Error("expected TypeError, got " + caught);
            }
        "#,
    )
    .unwrap();

    let otter = Otter::builder()
        .capabilities(CapabilitySet::sandbox())
        .build()
        .expect("otter");
    otter
        .run_module(entry)
        .await
        .expect("denial must surface as catchable TypeError");
}

/// On-demand load: a second `import(spec)` for the same specifier
/// must return the same namespace (fixed-point per §16.2.1.7).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_import_returns_cached_namespace_on_repeat() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("repeat.ts"), "export const value = 1;\n").unwrap();
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            const path = "./" + "repeat.ts";
            const a = await import(path);
            const b = await import(path);
            if (a !== b) {
                throw new Error("dynamic import re-loaded module");
            }
            if (a.value !== 1) {
                throw new Error("bad cached namespace: " + a.value);
            }
        "#,
    )
    .unwrap();

    run_module_async(&dir.path().join("entry.ts"))
        .await
        .expect("cached on-demand import must reuse namespace");
}

/// Non-string dynamic-import specifiers reject with a TypeError
/// per §16.2.1.7 step 7.b.i, instead of raising a host error.
#[test]
fn dynamic_import_with_non_string_specifier_rejects_with_typeerror() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("entry.ts"),
        r#"
            let caught = null;
            try {
                await import(42 as any);
            } catch (e) {
                caught = e;
            }
            if (!(caught instanceof TypeError)) {
                throw new Error("expected TypeError, got " + caught);
            }
        "#,
    )
    .unwrap();

    run_with_capabilities(&dir.path().join("entry.ts"), CapabilitySet::allow_all())
        .expect("non-string specifier must reject");
}
