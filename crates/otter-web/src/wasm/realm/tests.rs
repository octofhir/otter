//! Stable WebAssembly descriptors and real first-use backing in isolated realms.
//!
//! # Contents
//! - JSTag data-property identity before and after actual tag use.
//! - Per-realm initialization, compiled imports/exports and canonical errors.
//!
//! # Invariants
//! - The observer reads existing owned state and never starts Wasmtime itself.
//! - Native callbacks retain owned observations; assertions run outside the ABI.
//! - Every JS value is accessed through the public native handle scope.
//! - No timestamp, thread-count probe or injected initialization failure is used.

use super::*;
use crate::WebApiBuilderExt;
use otter_runtime::{
    Runtime, RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeValue,
    SourceInput,
};

struct Observation {
    owner: WasmRealm,
    backing: Option<(Engine, SharedStore)>,
}

fn runtime(observations: Arc<Mutex<Vec<Observation>>>) -> Runtime {
    Runtime::builder()
        .with_web_apis()
        .extension_installer(RuntimeExtensionInstaller::new(move |realm| {
            let observations = observations.clone();
            realm.install_native_global_call(
                "wasmInitialized",
                0,
                RuntimeNativeCall::Dynamic(Arc::new(
                    move |ctx: &mut RuntimeNativeCtx<'_>,
                          _args: &[RuntimeValue],
                          _captures: &[RuntimeValue]| {
                        ctx.scope(|scope| {
                            let mut cx = MarshalCx::new(scope);
                            let global = cx.global_this();
                            let carrier = cx
                                .get(global, REALM_KEY)
                                .map_err(|error| error.into_native("wasmInitialized"))?;
                            let owner = cx
                                .with_host_data::<WasmRealm, WasmRealm>(carrier, Clone::clone)
                                .map_err(|error| error.into_native("wasmInitialized"))?;
                            let backing = {
                                let state = owner
                                    .state
                                    .lock()
                                    .map_err(|_| RuntimeNativeError::InvalidOperand)?;
                                match &*state {
                                    RealmState::Uninitialized => None,
                                    RealmState::Ready { engine, store, .. } => {
                                        Some((engine.clone(), store.clone()))
                                    }
                                }
                            };
                            let initialized = backing.is_some();
                            observations
                                .lock()
                                .map_err(|_| RuntimeNativeError::InvalidOperand)?
                                .push(Observation { owner, backing });
                            let result = cx.boolean(initialized);
                            Ok(cx.escape(result))
                        })
                    },
                )),
            )?;
            Ok(())
        }))
        .build()
        .expect("runtime with WebAssembly")
}

#[test]
fn bootstrap_descriptors_and_jstag_identity_do_not_initialize_backing() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = runtime(observations.clone());
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
                const before = wasmInitialized();
                const tag = WebAssembly.JSTag;
                const descriptor = Object.getOwnPropertyDescriptor(WebAssembly, "JSTag");
                const cache = Object.getOwnPropertyDescriptor(globalThis, "__otterWasmRealm");
                const brand = tag instanceof WebAssembly.Tag;
                const afterReads = wasmInitialized();
                const signature = tag.type().parameters.join(",");
                const afterUse = wasmInitialized();
                const payload = { marker: 42 };
                const exception = new WebAssembly.Exception(tag, [payload]);
                const afterException = wasmInitialized();
                [before, afterReads, descriptor.value === tag, brand,
                 descriptor.writable, descriptor.enumerable, descriptor.configurable,
                 !descriptor.get && !descriptor.set,
                 !cache.writable && !cache.enumerable && !cache.configurable,
                 signature, afterUse, afterException, exception.is(tag),
                 exception.getArg(tag, 0) === payload, WebAssembly.JSTag === tag].join("|")
                "#,
            ),
            "wasm:first-use-descriptors",
        )
        .expect("descriptor and tag operations");
    assert_eq!(
        result.completion_string(),
        "false|false|true|true|false|false|true|true|true|externref|true|true|true|true|true"
    );
    let observations = observations.lock().expect("owned observations");
    assert_eq!(observations.len(), 4);
    assert!(observations[0].backing.is_none());
    assert!(observations[1].backing.is_none());
    for observation in &observations[1..] {
        assert!(Arc::ptr_eq(
            &observations[0].owner.state,
            &observation.owner.state
        ));
    }
    let (first_engine, first_store) = observations[2].backing.as_ref().expect("first use");
    let (later_engine, later_store) = observations[3].backing.as_ref().expect("later use");
    assert!(Engine::same(first_engine, later_engine));
    assert!(Arc::ptr_eq(first_store, later_store));
}

#[test]
fn first_use_is_realm_local_and_keeps_real_imports_exports_and_errors() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = runtime(observations.clone());
    let additional = runtime.create_realm().expect("additional realm bootstrap");
    let bytes = wat::parse_str(
        r#"(module
            (import "env" "memory" (memory 1))
            (import "env" "tag" (tag $tag (param externref)))
            (func (export "answer") (result i32) i32.const 42)
            (func (export "raise") (param externref) local.get 0 throw $tag))"#,
    )
    .expect("Wasm fixture");
    let bytes = bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        r#"
        const before = wasmInitialized();
        const bytes = new Uint8Array([{bytes}]);
        const valid = WebAssembly.validate(bytes);
        const after = wasmInitialized();
        const tag = WebAssembly.JSTag;
        const memory = new WebAssembly.Memory({{ initial: 1 }});
        const module = new WebAssembly.Module(bytes);
        const instance = new WebAssembly.Instance(module, {{ env: {{ memory, tag }} }});
        const sentinel = {{ marker: 42 }};
        let original = false;
        try {{ instance.exports.raise(sentinel); }} catch (error) {{ original = error === sentinel; }}
        let compileClass = false;
        try {{ new WebAssembly.Module(new Uint8Array([0])); }}
        catch (error) {{ compileClass = error instanceof WebAssembly.CompileError; }}
        [before, valid, after, instance.exports.answer(), original,
         !WebAssembly.validate(new Uint8Array([0])), compileClass,
         WebAssembly.JSTag === tag].join("|")
        "#,
    );
    let default = runtime
        .run_script(
            SourceInput::from_javascript(script.clone()),
            "wasm:default-first-use",
        )
        .expect("default realm first use");
    assert_eq!(
        default.completion_string(),
        "false|true|true|42|true|true|true|true"
    );
    let additional_result = runtime
        .run_script_in_realm(
            additional,
            SourceInput::from_javascript(script),
            "wasm:additional-first-use",
        )
        .expect("additional realm first use");
    assert_eq!(
        additional_result.completion_string(),
        default.completion_string()
    );
    let retained = runtime
        .run_script(
            SourceInput::from_javascript("wasmInitialized()"),
            "wasm:default-retained-owner",
        )
        .expect("repeated default use");
    assert_eq!(retained.completion_string(), "true");

    let observations = observations.lock().expect("owned observations");
    assert_eq!(observations.len(), 5);
    assert!(observations[0].backing.is_none());
    assert!(observations[2].backing.is_none());
    assert!(!Arc::ptr_eq(
        &observations[0].owner.state,
        &observations[2].owner.state
    ));
    assert!(Arc::ptr_eq(
        &observations[0].owner.state,
        &observations[1].owner.state
    ));
    assert!(Arc::ptr_eq(
        &observations[2].owner.state,
        &observations[3].owner.state
    ));
    let (default_engine, default_store) =
        observations[1].backing.as_ref().expect("default backing");
    let (additional_engine, additional_store) = observations[3]
        .backing
        .as_ref()
        .expect("additional backing");
    assert!(!Engine::same(default_engine, additional_engine));
    assert!(!Arc::ptr_eq(default_store, additional_store));
    let (retained_engine, retained_store) =
        observations[4].backing.as_ref().expect("retained backing");
    assert!(Engine::same(default_engine, retained_engine));
    assert!(Arc::ptr_eq(default_store, retained_store));
}
