//! Browser embedders execute modules from canonical URLs without temp files.

use std::collections::BTreeMap;
use std::sync::Arc;

use otter_runtime::module_loader::{RemoteModuleFetch, RemoteModuleSource};
use otter_runtime::{CapabilitySet, Permission, Runtime, SourceInput, TokioRuntimeHost};

#[derive(Debug)]
struct CachedModules {
    sources: BTreeMap<String, String>,
}

impl RemoteModuleFetch for CachedModules {
    fn fetch(&self, url: &str) -> Result<RemoteModuleSource, String> {
        let source = self
            .sources
            .get(url)
            .cloned()
            .ok_or_else(|| format!("cache miss for {url}"))?;
        Ok(RemoteModuleSource {
            source,
            content_type: Some("text/javascript".to_string()),
            final_url: url.to_string(),
        })
    }
}

#[test]
fn http_entry_and_static_dependency_execute_from_memory() {
    let mut capabilities = CapabilitySet::sandbox();
    capabilities.net = Permission::AllowAll;
    let mut runtime = Runtime::builder()
        .capabilities(capabilities)
        .build()
        .expect("runtime");
    runtime.install_remote_module_fetch(Arc::new(CachedModules {
        sources: BTreeMap::from([(
            "https://example.test/assets/dep.js".to_string(),
            "export const answer = 41;".to_string(),
        )]),
    }));

    runtime
        .run_module_source(
            SourceInput::from_javascript(
                "import { answer } from './assets/dep.js';\n\
                 globalThis.moduleSourceResult = answer + 1;\n\
                 globalThis.moduleSourceUrl = import.meta.url;",
            ),
            "https://example.test/index.js",
        )
        .expect("in-memory HTTP graph executes");

    assert_eq!(
        runtime
            .eval(SourceInput::from_javascript("moduleSourceResult"))
            .expect("read result")
            .completion_string(),
        "42"
    );
    assert_eq!(
        runtime
            .eval(SourceInput::from_javascript("moduleSourceUrl"))
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
