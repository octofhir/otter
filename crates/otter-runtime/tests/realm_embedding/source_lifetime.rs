//! Exact defining sources through URL reuse and runtime/code destruction.
//!
//! # Contents
//! - Default/additional classic-script and eval sources remain per-code owners.
//! - Owned diagnostics serialize exact lines and share admitted source leases.
//! - Actual eval source refusal leaves through the resource boundary before JS cleanup.
//!
//! # Invariants
//! - Every script is compiled/linked through the public real Runtime entry.
//! - No fixture depends on ambient mutable URL lookup or raw code/heap handles.
//! - Native pressure records owned observations; assertions stay outside the ABI.
//! - Source byte ownership may outlive executable/runtime ownership until the last diagnostic drops.
//!
//! # See also
//! - `otter_vm::source_registry` owns immutable per-chunk text/index metadata.
//! - `otter_runtime::script_source` admits genuine script sources before linking.

use otter_runtime::{
    JitSelection, NativeError, OtterError, ResourceAccount, ResourceClass, ResourceLease,
    ResourceLimits, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeRealmId,
    SourceInput,
};
use std::sync::{Arc, Mutex};

fn run(
    runtime: &mut Runtime,
    realm: Option<RuntimeRealmId>,
    source: &str,
    url: &str,
) -> Result<otter_runtime::ExecutionResult, OtterError> {
    match realm {
        Some(realm) => {
            runtime.run_script_in_realm(realm, SourceInput::from_javascript(source), url)
        }
        None => runtime.run_script(SourceInput::from_javascript(source), url),
    }
}

fn source_charge(account: &ResourceAccount) -> u64 {
    account
        .snapshot()
        .get(ResourceClass::SourceModuleBytes)
        .current()
}

fn frame<'a>(error: &'a OtterError, name: &str) -> &'a otter_runtime::StackFrame {
    let OtterError::Runtime { diagnostic } = error else {
        panic!("actual uncaught JS error: {error:?}")
    };
    diagnostic
        .frames
        .iter()
        .find(|frame| frame.function == name)
        .unwrap_or_else(|| panic!("exact defining function {name}: {:?}", diagnostic.frames))
}

#[test]
fn same_url_scripts_keep_the_old_function_source_in_default_and_additional_realms() {
    const ORIGINAL: &str = "function rememberedSource(){\n throw new Error(\"defined in A\");\n}\nglobalThis.oldSource = rememberedSource;";
    const ORIGINAL_LINE: &str = " throw new Error(\"defined in A\");";
    for additional in [false, true] {
        let account = ResourceAccount::default();
        let mut runtime = Runtime::builder()
            .resource_account(account.clone())
            .jit_selection(JitSelection::InterpreterOnly)
            .build()
            .expect("source runtime");
        let realm = additional.then(|| runtime.create_realm().expect("additional source realm"));
        run(&mut runtime, realm, ORIGINAL, "same-source.js").expect("define original function");
        run(&mut runtime, realm,
            "globalThis.sourceReplacement = 'B first line';\nglobalThis.sourceReplacement = 'B second line';",
            "same-source.js").expect("reuse exact source URL");
        let error = run(&mut runtime, realm, "oldSource();", "call-source.js")
            .expect_err("call original code");
        let original_frame = frame(&error, "rememberedSource");
        assert_eq!(original_frame.module, "same-source.js");
        let position = original_frame
            .source_position
            .as_ref()
            .expect("eager exact defining position");
        assert_eq!(position.script_name, "same-source.js");
        assert_eq!(position.line_number, 2);
        assert_eq!(position.source_line.as_ref(), ORIGINAL_LINE);
        let pointer = position.source_line.as_ref().as_ptr();
        let retained = error.clone();
        assert_eq!(
            frame(&retained, "rememberedSource")
                .source_position
                .as_ref()
                .unwrap()
                .source_line
                .as_ref()
                .as_ptr(),
            pointer
        );
        let before = source_charge(&account);
        let serialized: serde_json::Value =
            serde_json::from_str(&retained.to_json().unwrap()).unwrap();
        let stack = serialized["error"]["diagnostic"]["frames"]
            .as_array()
            .unwrap();
        let json_frame = stack
            .iter()
            .find(|frame| frame["function"] == "rememberedSource")
            .unwrap();
        assert_eq!(json_frame["source_position"]["source_line"], ORIGINAL_LINE);
        assert_eq!(
            source_charge(&account),
            before,
            "serialization/Clone do not admit duplicate text"
        );
        drop(error);
        drop(runtime);
        let kept = source_charge(&account);
        assert!(
            kept > 0,
            "owned positions outlive destroyed code/runtime without a ChunkPayload pin"
        );
        assert_eq!(
            frame(&retained, "rememberedSource")
                .source_position
                .as_ref()
                .unwrap()
                .source_line
                .as_ref(),
            ORIGINAL_LINE
        );
        let alias = retained.clone();
        assert_eq!(source_charge(&account), kept);
        drop(retained);
        assert_eq!(source_charge(&account), kept);
        drop(alias);
        assert_eq!(
            source_charge(&account),
            0,
            "last owned source handle releases its exact charge"
        );
    }
}

#[test]
fn repeated_eval_urls_do_not_replace_earlier_escaped_function_source() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .unwrap();
    run(&mut runtime, None,
        "globalThis.evalOld = eval(\"(function evalRemembered(){\\nthrow new Error('eval original A');\\n})\");",
        "eval-setup.js").unwrap();
    run(
        &mut runtime,
        None,
        "eval(\"'replacement B first line';\\n'replacement B second line';\");",
        "eval-replace.js",
    )
    .unwrap();
    let error = run(&mut runtime, None, "evalOld();", "eval-call.js").unwrap_err();
    let position = frame(&error, "evalRemembered")
        .source_position
        .as_ref()
        .unwrap();
    assert_eq!(
        position.source_line.as_ref(),
        "throw new Error('eval original A');"
    );
    assert_eq!(position.line_number, 2);
}

#[derive(Default)]
struct Pressure {
    lease: Option<ResourceLease>,
    observation: Option<(u64, u64)>,
}

#[test]
fn actual_eval_source_refusal_is_fatal_and_skips_catch_finally_and_body_effects() {
    const LIMIT: u64 = 16 * 1024 * 1024;
    const BODY: &str = "globalThis.dynamicResourceEffect = 'must not commit';";
    let account = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::SourceModuleBytes, LIMIT)
            .build(),
    );
    let pressure = Arc::new(Mutex::new(Pressure::default()));
    let callback_account = account.clone();
    let callback_pressure = pressure.clone();
    let mut runtime = Runtime::builder()
        .resource_account(account.clone())
        .jit_selection(JitSelection::InterpreterOnly)
        .extension_installer(RuntimeExtensionInstaller::new(move |realm| {
            let account = callback_account.clone();
            let pressure = callback_pressure.clone();
            realm.install_native_global_call(
                "fillSourceAccount",
                0,
                RuntimeNativeCall::Dynamic(Arc::new(move |_ctx, _args, _state| {
                    let before = source_charge(&account);
                    let lease = account
                        .reserve_exact(ResourceClass::SourceModuleBytes, LIMIT - before)
                        .map_err(|error| NativeError::Resource { error })?;
                    let after = source_charge(&account);
                    let mut record = pressure.lock().map_err(|_| NativeError::InvalidOperand)?;
                    record.observation = Some((before, after));
                    record.lease = Some(lease);
                    Ok(otter_runtime::Value::undefined())
                })),
            )?;
            Ok(())
        }))
        .build()
        .expect("admitted real source runtime");
    let script = format!(
        "fillSourceAccount(); try {{ eval({BODY:?}); }} catch (error) {{ globalThis.dynamicResourceCatch = true; }} finally {{ globalThis.dynamicResourceFinally = true; }}"
    );
    let error = run(&mut runtime, None, &script, "resource-eval.js")
        .expect_err("actual source owner refuses dynamic body");
    let OtterError::Resource { error } = error else {
        panic!("exact engine resource boundary")
    };
    assert_eq!(error.class(), ResourceClass::SourceModuleBytes);
    assert_eq!(error.requested(), BODY.len() as u64);
    assert_eq!(error.in_use(), Some(LIMIT));
    assert_eq!(error.limit(), Some(LIMIT));
    let mut record = pressure.lock().unwrap();
    let (before, after) = record.observation.unwrap();
    assert!(before < LIMIT);
    assert_eq!(after, LIMIT);
    drop(record.lease.take());
    drop(record);
    let recovered = run(&mut runtime, None,
        "typeof dynamicResourceEffect + ':' + typeof dynamicResourceCatch + ':' + typeof dynamicResourceFinally",
        "resource-recovery.js").expect("later explicit host turn");
    assert_eq!(
        recovered.completion_string(),
        "undefined:undefined:undefined"
    );
    drop(runtime);
    assert_eq!(source_charge(&account), 0);
}
