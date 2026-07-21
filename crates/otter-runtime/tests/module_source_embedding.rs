//! Browser embedders execute modules from canonical URLs without temp files.

use otter_runtime::module_loader::{
    RemoteModuleError, RemoteModuleFuture, RemoteModuleProvider, RemoteModuleRequest,
    RemoteModuleSource,
};
use otter_runtime::{CapabilitySet, Otter, Permission, Runtime, SourceInput, TokioRuntimeHost};
use std::collections::BTreeMap;

#[derive(Debug)]
struct CachedModules {
    sources: BTreeMap<String, String>,
}

impl RemoteModuleProvider for CachedModules {
    fn fetch(&self, request: RemoteModuleRequest) -> RemoteModuleFuture {
        let source = self.sources.get(&request.url).cloned();
        Box::pin(async move {
            let source = source.ok_or_else(|| RemoteModuleError::Fetch {
                url: request.url.clone(),
                message: "cache miss".to_string(),
            })?;
            Ok(RemoteModuleSource {
                source,
                content_type: Some("text/javascript".to_string()),
                final_url: request.url,
            })
        })
    }
}

#[tokio::test]
async fn http_entry_and_static_dependency_execute_from_memory() {
    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::AllowAll;
    let runtime = Otter::builder()
        .capabilities(capabilities)
        .remote_module_provider(CachedModules {
            sources: BTreeMap::from([(
                "https://example.test/assets/dep.js".to_string(),
                "export const answer = 41;".to_string(),
            )]),
        })
        .build()
        .expect("runtime");

    runtime
        .run_module_source(
            SourceInput::from_javascript(
                "import { answer } from './assets/dep.js';\n\
                 globalThis.moduleSourceResult = answer + 1;\n\
                 globalThis.moduleSourceUrl = import.meta.url;",
            ),
            "https://example.test/index.js",
        )
        .await
        .expect("in-memory HTTP graph executes");

    assert_eq!(
        runtime
            .eval("moduleSourceResult")
            .await
            .expect("read result")
            .completion_string(),
        "42"
    );
    assert_eq!(
        runtime
            .eval("moduleSourceUrl")
            .await
            .expect("read URL")
            .completion_string(),
        "https://example.test/index.js"
    );
}

#[test]
fn malformed_entry_url_fails_before_execution() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let error = runtime
        .run_module_source(
            SourceInput::from_javascript("globalThis.mustNotRun = true"),
            "relative/module.js",
        )
        .expect_err("entry URL must be canonical");
    assert!(format!("{error:?}").contains("absolute"));
    assert_eq!(
        runtime
            .eval(SourceInput::from_javascript("typeof globalThis.mustNotRun",))
            .expect("runtime remains usable")
            .completion_string(),
        "undefined"
    );
}

#[test]
fn sendable_page_isolate_accepts_an_in_memory_module_entry() {
    let host = TokioRuntimeHost::new().expect("Tokio host");
    let page = Runtime::builder()
        .runtime_host(host.clone())
        .build_handle()
        .expect("page isolate");

    host.handle()
        .block_on(page.run_module_source(
            SourceInput::from_javascript("globalThis.asyncModuleUrl = import.meta.url;"),
            "https://browser.test/page-module.js",
        ))
        .expect("module executes through the isolate inbox");
    let result = host
        .handle()
        .block_on(page.eval(SourceInput::from_javascript("asyncModuleUrl")))
        .expect("read module side effect");
    assert_eq!(
        result.completion_string(),
        "https://browser.test/page-module.js"
    );
}

#[tokio::test]
async fn module_graph_executes_only_in_its_target_realm() {
    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::AllowAll;
    let runtime = Otter::builder()
        .capabilities(capabilities)
        .remote_module_provider(CachedModules {
            sources: BTreeMap::from([(
                "https://realm.test/dep.js".to_string(),
                "export const answer = 41;".to_string(),
            )]),
        })
        .build()
        .expect("runtime");
    let first = runtime.create_realm().await.expect("first realm");
    let second = runtime.create_realm().await.expect("second realm");

    runtime
        .run_module_source_in_realm(
            first,
            SourceInput::from_javascript(
                "import { answer } from './dep.js'; globalThis.realmModule = answer + 1; globalThis.realmModuleUrl = import.meta.url;",
            ),
            "https://realm.test/entry.js",
        )
        .await
        .expect("first realm module");
    assert_eq!(
        runtime
            .run_script_in_realm(
                first,
                SourceInput::from_javascript("realmModule + ':' + realmModuleUrl"),
                "realm:first-check",
            )
            .await
            .expect("first realm result")
            .completion_string(),
        "42:https://realm.test/entry.js"
    );
    assert_eq!(
        runtime
            .run_script("typeof realmModule")
            .await
            .expect("default realm remains isolated")
            .completion_string(),
        "undefined"
    );
    assert_eq!(
        runtime
            .run_script_in_realm(
                second,
                SourceInput::from_javascript("typeof realmModule"),
                "realm:second-check",
            )
            .await
            .expect("second realm remains isolated")
            .completion_string(),
        "undefined"
    );
}

#[test]
fn sendable_isolate_routes_module_to_opaque_realm() {
    let host = TokioRuntimeHost::new().expect("Tokio host");
    let page = Runtime::builder()
        .runtime_host(host.clone())
        .build_handle()
        .expect("page isolate");
    let realm = host.handle().block_on(page.create_realm()).expect("realm");
    host.handle()
        .block_on(page.run_module_source_in_realm(
            realm,
            SourceInput::from_javascript(
                "globalThis.realmModuleUrl = import.meta.url; globalThis.realmModuleValue = 42;",
            ),
            "https://browser.test/realm-module.js",
        ))
        .expect("realm module executes through isolate inbox");
    let result = host
        .handle()
        .block_on(page.run_script_in_realm(
            realm,
            SourceInput::from_javascript("realmModuleValue + ':' + realmModuleUrl"),
            "realm:module-result",
        ))
        .expect("read realm module side effect");
    assert_eq!(
        result.completion_string(),
        "42:https://browser.test/realm-module.js"
    );
}

#[tokio::test]
async fn canonical_module_map_persists_per_realm_across_entry_graphs() {
    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::AllowAll;
    let runtime = Otter::builder()
        .capabilities(capabilities)
        .remote_module_provider(CachedModules {
            sources: BTreeMap::from([(
                "https://modules.test/shared.js".to_string(),
                "globalThis.sharedRuns = (globalThis.sharedRuns || 0) + 1; export const value = sharedRuns;"
                    .to_string(),
            )]),
        })
        .build()
        .expect("runtime");
    let first = runtime.create_realm().await.expect("first realm");
    let second = runtime.create_realm().await.expect("second realm");

    for (url, target) in [
        ("https://modules.test/first.js", "firstValue"),
        ("https://modules.test/second.js", "secondValue"),
    ] {
        runtime
            .run_module_source_in_realm(
                first,
                SourceInput::from_javascript(format!(
                    "import {{ value }} from './shared.js'; globalThis.{target} = value;"
                )),
                url,
            )
            .await
            .expect("entry graph");
    }
    assert_eq!(
        runtime
            .run_script_in_realm(
                first,
                SourceInput::from_javascript("sharedRuns + ':' + firstValue + ':' + secondValue"),
                "realm:first-module-map",
            )
            .await
            .expect("first module map")
            .completion_string(),
        "1:1:1"
    );

    runtime
        .run_module_source_in_realm(
            second,
            SourceInput::from_javascript(
                "import { value } from './shared.js'; globalThis.secondRealmValue = value;",
            ),
            "https://modules.test/other.js",
        )
        .await
        .expect("second realm graph");
    assert_eq!(
        runtime
            .run_script_in_realm(
                second,
                SourceInput::from_javascript("sharedRuns + ':' + secondRealmValue"),
                "realm:second-module-map",
            )
            .await
            .expect("second module map")
            .completion_string(),
        "1:1",
        "the same URL has an independent module record in another realm"
    );
}
