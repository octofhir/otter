//! Exact Web static source, realm, hook and descriptor boundaries.
//!
//! # Contents
//! Actual default/additional bootstrap, retained source diagnostics and hook input.
//!
//! # Invariants
//! All observations enter through public Runtime APIs; no heap/context handle
//! or generated status is fabricated. Hooks own only Send/Sync Rust observations.
//!
//! # See also
//! - `otter_runtime::ExtensionJs` owns the build-produced script.

use otter_runtime::{OtterError, Runtime, RuntimeCompileRequest, RuntimeRealmId, SourceInput};
use otter_web::{WEB_EXTENSION, WebApiBuilderExt};
use std::sync::{Arc, Mutex};

fn run(
    runtime: &mut Runtime,
    realm: Option<RuntimeRealmId>,
    text: &str,
) -> Result<otter_runtime::ExecutionResult, OtterError> {
    let source = SourceInput::from_javascript(text);
    match realm {
        None => runtime.run_script(source, "web-bootstrap-proof.js"),
        Some(realm) => runtime.run_script_in_realm(realm, source, "web-bootstrap-proof.js"),
    }
}

#[test]
fn actual_asset_installs_descriptors_in_both_realms_and_retains_defining_source() {
    let asset = WEB_EXTENSION.js.expect("static Web bundle");
    let names = WEB_EXTENSION
        .defined_names()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(",");
    let observation = format!(
        "[{}].every(name => {{ const d = Object.getOwnPropertyDescriptor(globalThis, name); return !!d && !d.enumerable; }}) && Object.getPrototypeOf(CustomEvent.prototype) === Event.prototype && Object.getPrototypeOf(File.prototype) === Blob.prototype && new TextEncoder().encode('ok').length === 2 && self === globalThis",
        names
    );
    assert!(asset.source.ends_with("\n;\n"));
    let mut runtime = Runtime::builder()
        .with_web_apis()
        .build()
        .expect("static Web default");
    let realm = runtime.create_realm().expect("static Web additional");
    let mut errors = Vec::new();
    for realm in [None, Some(realm)] {
        assert_eq!(
            run(&mut runtime, realm, &observation)
                .expect("live descriptors")
                .completion_string(),
            "true"
        );
        let error = run(&mut runtime, realm, "new Event();").expect_err("real shim throw");
        let OtterError::Runtime { diagnostic } = &error else {
            panic!("shim completion: {error:?}")
        };
        let specifier = if realm.is_none() {
            "<bootstrap:web>"
        } else {
            "<realm-installer>"
        };
        let frame = diagnostic
            .frames
            .iter()
            .find(|frame| frame.module == specifier)
            .expect("actual Web defining frame");
        let position = frame
            .source_position
            .as_ref()
            .expect("owned source position");
        assert_eq!(position.script_name, specifier);
        assert!(
            position
                .source_line
                .as_ref()
                .contains("Failed to construct 'Event'")
        );
        errors.push(error);
    }
    drop(runtime);
    for error in errors {
        let OtterError::Runtime { diagnostic } = error else {
            unreachable!()
        };
        assert!(
            diagnostic
                .frames
                .iter()
                .filter_map(|frame| frame.source_position.as_ref())
                .any(|position| position
                    .source_line
                    .as_ref()
                    .contains("Failed to construct 'Event'"))
        );
    }
}

#[test]
fn configured_hook_receives_exact_web_bundle_and_metadata_in_both_realms() {
    let script = WEB_EXTENSION.js.expect("Web asset");
    let observations = Arc::new(Mutex::new(Vec::new()));
    let observed = observations.clone();
    let mut runtime = Runtime::builder()
        .with_web_apis()
        .compile_hook(move |request: RuntimeCompileRequest<'_>| {
            let compiled = otter_compiler::compile_script_source_to_module(
                request.source.text.as_ref(),
                request.source.kind,
                &request.source.url,
            )
            .expect("actual hook frontend");
            if request.source.text.as_ref() == script.source {
                observed.lock().unwrap().push((
                    request.source.url.clone(),
                    request.source.kind,
                    request.source.text.to_string(),
                    compiled.metadata.source_url.clone(),
                    compiled.metadata.function_spans.len(),
                ));
            }
            Ok(compiled)
        })
        .build()
        .expect("hook default Web");
    let realm = runtime.create_realm().expect("hook additional Web");
    for realm in [None, Some(realm)] {
        assert_eq!(
            run(
                &mut runtime,
                realm,
                "new Event('hook').type + ':' + (new Headers() instanceof Headers)"
            )
            .expect("hook-installed surface")
            .completion_string(),
            "hook:true"
        );
    }
    let observations = observations.lock().unwrap();
    assert_eq!(
        observations.len(),
        2,
        "one complete script per extension and realm"
    );
    for (index, (url, kind, source, metadata_url, spans)) in observations.iter().enumerate() {
        assert_eq!(
            url,
            if index == 0 {
                "<bootstrap:web>"
            } else {
                "<realm-installer>"
            }
        );
        assert_eq!(*kind, otter_syntax::SourceKind::JavaScript);
        assert_eq!(source, script.source);
        assert_eq!(metadata_url, url);
        assert!(*spans > 0);
    }
    drop(observations);
    let refusal = Runtime::builder()
        .with_web_apis()
        .compile_hook(move |request: RuntimeCompileRequest<'_>| {
            if request.source.text.as_ref() == script.source {
                return Err(OtterError::Usage {
                    message: "exact Web hook refusal".to_owned(),
                });
            }
            otter_compiler::compile_script_source_to_module(
                request.source.text.as_ref(),
                request.source.kind,
                &request.source.url,
            )
            .map_err(|error| OtterError::Usage {
                message: error.to_string(),
            })
        })
        .build();
    let error = match refusal {
        Ok(_) => panic!("embedded code cannot bypass hook refusal"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("exact Web hook refusal"));
}
