//! Static admission, hook and extension-order proofs.
//!
//! # Contents
//! Real linking refuses invalid assets/resource pressure and honors hook output.
//!
//! # Invariants
//! Fixture leaks are bounded static descriptor bytes, never VM values or roots.
//! Every executed script enters the existing Runtime bootstrap owner.

use super::*;
use crate::{
    Extension, ResourceAccount, ResourceClass, ResourceLimits, Runtime, RuntimeGlobalInstaller,
    SourceInput,
};
use std::sync::{Arc, Mutex};

fn asset(source: &'static str) -> ExtensionJs {
    let compiled = otter_compiler::compile_script_source_to_module(
        source,
        otter_syntax::SourceKind::JavaScript,
        "<asset-test>",
    )
    .expect("fixture script");
    let bytes = otter_bytecode::binary::encode_module_bounded(&compiled.bytecode, 64 * 1024)
        .expect("bounded fixture asset");
    ExtensionJs {
        source,
        bytecode: Box::leak(bytes.into_boxed_slice()),
        defines: &[],
    }
}

#[test]
fn malformed_asset_and_source_refusal_do_not_publish_code_or_globals() {
    let account = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::SourceModuleBytes, 0)
            .build(),
    );
    let mut interp = Interpreter::new().expect("fixture isolate");
    interp
        .set_resource_account(account.clone())
        .expect("resource owner");
    let script = asset("globalThis.refusedStatic = 1;");
    assert!(matches!(
        prepare(&mut interp, &RuntimeHooks::default(), &script, "<refused>"),
        Err(OtterError::Resource { .. })
    ));
    assert_eq!(
        account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .current(),
        0
    );
    assert!(otter_vm::NativeCtx::with_host_context(
        &mut interp,
        otter_vm::NativeCallInfo::default_call(),
        None,
        |ctx| ctx.scope(|mut scope| scope.global("refusedStatic").is_none())
    ));
    let mut interp = Interpreter::new().expect("fixture isolate");
    let script = ExtensionJs {
        bytecode: b"not bytecode",
        ..script
    };
    assert!(matches!(
        prepare(&mut interp, &RuntimeHooks::default(), &script, "<invalid>"),
        Err(OtterError::Internal { .. })
    ));
    assert!(otter_vm::NativeCtx::with_host_context(
        &mut interp,
        otter_vm::NativeCallInfo::default_call(),
        None,
        |ctx| ctx.scope(|mut scope| scope.global("refusedStatic").is_none())
    ));
}

#[test]
fn hooks_preserve_whole_sources_metadata_and_per_extension_order() {
    const A: &str = "let bundleLexical = 7;\n;\nglobalThis.bundleOrder += 'A'; globalThis.bundleValue = bundleLexical;\n;\n";
    const B: &str = "globalThis.bundleOrder += 'B'; globalThis.bundleValue += 2;\n;\n";
    let first = Box::leak(Box::new(Extension {
        name: "first",
        classes: &[],
        js: Some(asset(A)),
    }));
    // An invalid default asset is legal here only because the configured hook
    // owns compilation. The default path above rejects this exact condition.
    let second = Box::leak(Box::new(Extension {
        name: "second",
        classes: &[],
        js: Some(ExtensionJs {
            source: B,
            bytecode: b"not consulted with a hook",
            defines: &[],
        }),
    }));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let mut runtime = Runtime::builder()
        .global_installer(RuntimeGlobalInstaller::new(|realm| {
            realm.install_script(SourceInput::from_javascript(
                "globalThis.bundleOrder = 'I';",
            ))
        }))
        .extension(first)
        .extension(second)
        .compile_hook(move |request: RuntimeCompileRequest<'_>| {
            let compiled = otter_compiler::compile_script_source_to_module(
                request.source.text.as_ref(),
                request.source.kind,
                &request.source.url,
            )
            .expect("hook source");
            if request.source.text.as_ref() == A || request.source.text.as_ref() == B {
                observed.lock().unwrap().push((
                    request.source.url.clone(),
                    request.source.text.to_string(),
                    compiled.metadata.source_url.clone(),
                    compiled.metadata.function_spans.len(),
                ));
            }
            Ok(compiled)
        })
        .build()
        .expect("hook bootstrap");
    let realm = runtime.create_realm().expect("hook additional realm");
    for realm in [None, Some(realm)] {
        let source = SourceInput::from_javascript("bundleOrder + ':' + bundleValue");
        let result = match realm {
            None => runtime.run_script(source, "bundle-proof.js"),
            Some(realm) => runtime.run_script_in_realm(realm, source, "bundle-proof.js"),
        }
        .expect("observe order");
        assert_eq!(result.completion_string(), "IAB:9");
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    for (i, (url, source, metadata_url, spans)) in seen.iter().enumerate() {
        assert_eq!(source, if i % 2 == 0 { A } else { B });
        assert_eq!(
            url,
            if i < 2 {
                if i == 0 {
                    "<bootstrap:first>"
                } else {
                    "<bootstrap:second>"
                }
            } else {
                "<realm-installer>"
            }
        );
        assert_eq!(metadata_url, url);
        assert!(*spans > 0);
    }
}
