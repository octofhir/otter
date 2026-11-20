//! Compiler hooks, source maps and the bootstrap cache.
//!
//! # Contents
//! Regression tests through the runtime embedding API.
//!
//! # Invariants
//! Each test constructs its own runtime.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use super::*;

const COMPILE_HOOK_CACHE_TEST_NAME: &str = "compile-hook-cache-regression";

fn assert_send_sync<T: Send + Sync>() {}

fn marker_compile_hook(
    marker: u32,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> impl RuntimeCompileHook {
    move |request: RuntimeCompileRequest<'_>| {
        let target = format!("<bootstrap:{COMPILE_HOOK_CACHE_TEST_NAME}>");
        let replacement = (request.source.url == target).then(|| {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            format!("globalThis.__otterCompileHookCacheMarker = {marker};")
        });
        let text = replacement.as_deref().unwrap_or(&request.source.text);
        otter_compiler::compile_script_source_to_module(
            text,
            request.source.kind,
            &request.source.url,
        )
        .map_err(|error| map_compile_error(error, &request.source.url))
    }
}

fn source_map_metadata(
    source_url: &str,
    spans: &[(u32, u32, (u32, u32))],
) -> CompiledModuleMetadata {
    let mut metadata =
        otter_compiler::compile_script_source_to_module("", SourceKind::JavaScript, source_url)
            .expect("empty source compiles")
            .metadata;
    metadata.source_url = source_url.to_string();
    let mut by_function = BTreeMap::<u32, Vec<SpanEntry>>::new();
    for &(function_id, pc, span) in spans {
        by_function
            .entry(function_id)
            .or_default()
            .push(SpanEntry { pc, span });
    }
    metadata.function_spans = by_function
        .into_iter()
        .map(|(function_id, spans)| CompiledFunctionSpans {
            function_id,
            function_name: format!("f{function_id}"),
            module_url: source_url.to_string(),
            spans,
        })
        .collect();
    metadata
}

#[test]
fn default_jit_selection_is_production_tiered() {
    assert_eq!(JitSelection::default(), JitSelection::ProductionTiered);
}

#[test]
fn public_handle_types_are_send_sync() {
    assert_send_sync::<Otter>();
    assert_send_sync::<RuntimeHandle>();
    assert_send_sync::<RuntimeHooks>();
}

#[test]
fn runtime_diagnostic_hook_receives_emit() {
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = count.clone();
    let runtime = Runtime::builder()
        .diagnostic_hook(move |diagnostic: &Diagnostic| {
            if diagnostic.code == "SYNTAX_ERROR" {
                seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .build()
        .unwrap();

    runtime.emit_diagnostic(&Diagnostic::syntax("resolver failed"));

    assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn runtime_capability_hook_can_override_default_policy() {
    struct AllowExampleNetAndEnv;

    impl RuntimeCapabilityHook for AllowExampleNetAndEnv {
        fn check_capability(
            &self,
            _capabilities: &CapabilitySet,
            capability: RuntimeCapability,
            request: &CapabilityRequest<'_>,
        ) -> bool {
            (capability == RuntimeCapability::Net
                && matches!(
                    request,
                    CapabilityRequest::Network { url, initiator: None }
                        if url.as_str() == "https://example.com/module.js"
                ))
                || capability == RuntimeCapability::Env
        }
    }

    let runtime = Runtime::builder()
        .capabilities(CapabilitySet::sandbox())
        .capability_hook(AllowExampleNetAndEnv)
        .build()
        .unwrap();

    let url = url::Url::parse("https://example.com/module.js").unwrap();
    assert!(runtime.check_capability(
        RuntimeCapability::Net,
        &CapabilityRequest::Network {
            url: &url,
            initiator: None,
        }
    ));
    assert!(runtime.check_capability(RuntimeCapability::Env, &CapabilityRequest::EnvVar("HOME")));
    assert!(!runtime.check_capability(
        RuntimeCapability::Env,
        &CapabilityRequest::EnvVar("OPENAI_API_KEY")
    ));
}

#[test]
fn runtime_session_owns_module_graph_and_source_map_state() {
    let dir = tempfile::tempdir().unwrap();
    let dep = dir.path().join("dep.ts");
    let entry = dir.path().join("entry.ts");
    std::fs::write(&dep, "export const value = 1;\n").unwrap();
    std::fs::write(
        &entry,
        "import { value } from './dep.ts';\nexport const result = value + 1;\n",
    )
    .unwrap();

    let mut runtime = Runtime::builder().build().unwrap();
    runtime.check_file(&entry).unwrap();

    let entry_url = format!(
        "file://{}",
        std::fs::canonicalize(&entry).unwrap().display()
    );
    let dep_url = format!("file://{}", std::fs::canonicalize(&dep).unwrap().display());
    assert_eq!(
        runtime.module_graph.last_entry_url.as_deref(),
        Some(entry_url.as_str())
    );
    assert_eq!(runtime.module_graph.last_module_count, 2);
    assert!(runtime.source_maps.contains_module(&entry_url));
    assert!(runtime.source_maps.contains_module(&dep_url));
}

#[test]
fn source_map_recompilation_replaces_stale_functions_and_spans() {
    let table = RuntimeSourceMapTable::default();
    let url = "file:///source-map-reload.js";
    let first = source_map_metadata(url, &[(0, 0, (0, 1)), (0, 10, (10, 11)), (1, 0, (20, 21))]);
    table.record_compiled_metadata(&first);
    table.record_compiled_metadata(&first);

    {
        let sources = table.by_module_url.borrow();
        let functions = sources.get(url).expect("source recorded");
        assert_eq!(functions.len(), 2);
        assert_eq!(functions.get(&0).expect("function zero").len(), 2);
    }

    let replacement = source_map_metadata(url, &[(0, 5, (100, 110)), (0, 0, (90, 95))]);
    table.record_compiled_metadata(&replacement);
    assert_eq!(table.resolve_frame_span(url, 0, 0), Some((90, 95)));
    assert_eq!(table.resolve_frame_span(url, 0, 9), Some((100, 110)));
    assert_eq!(table.resolve_frame_span(url, 1, 0), None);

    let empty = source_map_metadata(url, &[]);
    table.record_compiled_metadata(&empty);
    assert!(table.contains_module(url));
    assert_eq!(table.resolve_frame_span(url, 0, 0), None);
    assert!(
        table
            .by_module_url
            .borrow()
            .get(url)
            .expect("empty replacement remains authoritative")
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn compile_hooks_bypass_and_never_publish_to_the_shared_bootstrap_cache() {
    let directory = tempfile::tempdir().unwrap();
    let cache = compile_cache::CompileCache::new(directory.path().join("cache"));
    let source = "globalThis.__otterCompileHookCacheMarker = 0;";
    let specifier = format!("<bootstrap:{COMPILE_HOOK_CACHE_TEST_NAME}>");
    let key = compile_cache::cache_key(source, SourceKind::JavaScript, &specifier);
    let seed =
        otter_compiler::compile_script_source_to_module(source, SourceKind::JavaScript, &specifier)
            .expect("default compiler seed");
    cache.store(&key, &seed.bytecode);
    assert!(cache.load(&key, "").is_some());

    for marker in [1_u32, 2] {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut runtime = Runtime::builder()
            .compile_hook(marker_compile_hook(marker, calls.clone()))
            .build()
            .expect("runtime with compile hook");
        runtime
            .run_bootstrap_script_with_cache(
                COMPILE_HOOK_CACHE_TEST_NAME,
                source,
                Some(cache.clone()),
            )
            .expect("hook-produced bootstrap executes");
        let observed = runtime
            .eval(SourceInput::from_javascript(
                "globalThis.__otterCompileHookCacheMarker;",
            ))
            .expect("read hook marker");

        assert_eq!(observed.completion_string(), marker.to_string());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    let retained = cache.load(&key, "").expect("default cache entry remains");
    assert_eq!(
        otter_bytecode::binary::encode_module(retained.module()),
        otter_bytecode::binary::encode_module(&seed.bytecode)
    );
}

#[cfg(unix)]
#[test]
fn bootstrap_cache_hit_restores_cold_compile_frame_spans() {
    let directory = tempfile::tempdir().unwrap();
    let cache = compile_cache::CompileCache::new(directory.path().join("cache"));
    let name = "source-map-cache-parity";
    let source = "function add(a, b) { return a + b; }\nglobalThis.__cacheSpan = add(20, 22);";
    let specifier = format!("<bootstrap:{name}>");
    let key = compile_cache::cache_key(source, SourceKind::JavaScript, &specifier);

    let mut cold = Runtime::builder().build().expect("cold runtime");
    cold.run_bootstrap_script_with_cache(name, source, Some(cache.clone()))
        .expect("cold bootstrap");
    assert!(cache.load(&key, "").is_some());
    let cold_maps = cold.source_maps.by_module_url.borrow().clone();
    let functions = cold_maps.get(&specifier).expect("cold source map");
    let (&function_id, spans) = functions
        .iter()
        .find(|(_, spans)| !spans.is_empty())
        .expect("compiled bootstrap has spans");
    let pc = spans[0].pc;
    let cold_span = cold
        .resolve_frame_span(&specifier, function_id, pc)
        .expect("cold frame span");

    let mut hit = Runtime::builder().build().expect("cache-hit runtime");
    hit.run_bootstrap_script_with_cache(name, source, Some(cache))
        .expect("cached bootstrap");

    assert_eq!(
        hit.source_maps
            .by_module_url
            .borrow()
            .get(&specifier)
            .cloned(),
        Some(functions.clone())
    );
    assert_eq!(
        hit.resolve_frame_span(&specifier, function_id, pc),
        Some(cold_span)
    );
}

#[cfg(unix)]
#[test]
fn over_budget_cache_metadata_falls_back_without_partial_source_map_installation() {
    let directory = tempfile::tempdir().unwrap();
    let cache = compile_cache::CompileCache::new(directory.path().join("cache"));
    let name = "source-map-cache-budget-fallback";
    let source = "globalThis.__cacheMetadataFallback = 1;";
    let specifier = format!("<bootstrap:{name}>");
    let key = compile_cache::cache_key(source, SourceKind::JavaScript, &specifier);

    let mut hostile = otter_compiler::compile_script_source_to_module(
        "globalThis.__cacheMetadataFallback = 99;",
        SourceKind::JavaScript,
        &specifier,
    )
    .expect("hostile cache seed compiles");
    hostile.bytecode.functions[0].name = "cached".repeat(1_024);
    cache.store(&key, &hostile.bytecode);
    assert!(cache.load(&key, "").is_some());

    let mut expected = Runtime::builder().build().expect("reference runtime");
    expected
        .run_bootstrap_script_with_cache(name, source, None)
        .expect("reference cold bootstrap");
    let expected_maps = expected.source_maps.by_module_url.borrow().clone();

    let mut fallback = Runtime::builder().build().expect("fallback runtime");
    let stale = source_map_metadata(&specifier, &[(77, 0, (900, 901))]);
    fallback.source_maps.record_compiled_metadata(&stale);
    fallback
        .run_bootstrap_script_with_cache_and_metadata_budget(
            name,
            source.to_string(),
            Some(cache),
            128,
        )
        .expect("over-budget hit falls back to cold compilation");
    assert_eq!(
        fallback.source_maps.by_module_url.borrow().clone(),
        expected_maps,
        "failed cached reconstruction must install neither cached nor stale partial spans"
    );
    let observed = fallback
        .eval(SourceInput::from_javascript(
            "globalThis.__cacheMetadataFallback;",
        ))
        .expect("read fallback marker");

    assert_eq!(observed.completion_string(), "1");
}

#[test]
fn dump_file_uses_module_graph_and_preserves_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let dep = dir.path().join("dep.ts");
    let entry = dir.path().join("entry.ts");
    std::fs::write(&dep, "export const value = 1;\n").unwrap();
    std::fs::write(
        &entry,
        "import { value } from './dep.ts';\nexport const result = value + 1;\n",
    )
    .unwrap();

    let mut runtime = Runtime::builder().build().unwrap();
    let compiled = runtime.dump_file(&entry).unwrap();

    let entry_url = format!(
        "file://{}",
        std::fs::canonicalize(&entry).unwrap().display()
    );
    let dep_url = format!("file://{}", std::fs::canonicalize(&dep).unwrap().display());
    assert_eq!(compiled.entry_url.as_deref(), Some(entry_url.as_str()));
    assert_eq!(compiled.metadata.len(), 2);
    assert!(compiled.metadata.iter().any(|metadata| {
        metadata.source_url == entry_url
            && metadata.imports.iter().any(|import| {
                import.specifier == "./dep.ts" && import.target.as_deref() == Some(dep_url.as_str())
            })
            && metadata
                .exports
                .iter()
                .any(|export| export.name == "result")
    }));
    assert!(runtime.source_maps.contains_module(&entry_url));
    assert!(runtime.source_maps.contains_module(&dep_url));
}

#[test]
fn run_module_allocates_records_before_evaluation() {
    let dir = tempfile::tempdir().unwrap();
    let dep = dir.path().join("dep.ts");
    let entry = dir.path().join("entry.ts");
    std::fs::write(&dep, "export const value = 1;\n").unwrap();
    std::fs::write(
        &entry,
        "import { value } from './dep.ts';\nif (value !== 1) undefined.x;\n",
    )
    .unwrap();

    let mut runtime = Runtime::builder().build().unwrap();
    runtime.run_module(&entry).unwrap();

    let entry_url = format!(
        "file://{}",
        std::fs::canonicalize(&entry).unwrap().display()
    );
    let dep_url = format!("file://{}", std::fs::canonicalize(&dep).unwrap().display());
    assert_eq!(runtime.module_records.len(), 2);
    assert_eq!(
        runtime.module_records.state(&entry_url),
        Some(module_records::RuntimeModuleRecordState::Evaluated)
    );
    assert_eq!(
        runtime.module_records.state(&dep_url),
        Some(module_records::RuntimeModuleRecordState::Evaluated)
    );
    assert!(runtime.interp.module_env(&entry_url).is_some());
    assert!(runtime.interp.module_env(&dep_url).is_some());
}
