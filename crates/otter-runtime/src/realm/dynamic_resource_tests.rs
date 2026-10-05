//! Completed dynamic-module resource failures in their actual realm.
//!
//! # Contents
//! - Genuine module compilation/evaluation exercises both dynamic-load owners.
//! - An admitted native fills the source ledger immediately before observable eval.
//!
//! # Invariants
//! - Assertions execute after native return; callbacks record owned observations.
//! - No source refusal becomes a catchable module rejection or starts JS cleanup.
//! - Both owners return the original resource cause through the one runtime mapper.
//!
//! # See also
//! - `crate::evaluate_dynamic_linked_module_on` owns additional-realm evaluation.
//! - `otter_vm::NativeCtx::evaluate_module` transfers its existing owned RunError.

use super::RuntimeRealmId;
use crate::{
    DynLoadError, JitSelection, NativeError, OtterError, ResourceAccount, ResourceClass,
    ResourceLease, ResourceLimits, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall,
    SourceInput, module_graph, module_loader,
};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Pressure {
    lease: Option<ResourceLease>,
    observed: Option<(u64, u64)>,
    calls: usize,
}

fn charge(account: &ResourceAccount) -> u64 {
    account
        .snapshot()
        .get(ResourceClass::SourceModuleBytes)
        .current()
}

fn linked(
    runtime: &Runtime,
    account: &ResourceAccount,
    source: &str,
    url: &str,
) -> module_graph::LinkedProgram {
    let loader = runtime.module_loader_for_entry(Path::new("."));
    let source = SourceInput::from_javascript(source);
    module_graph::load_program_source(
        &loader,
        module_loader::ResolvedSource {
            url: url.to_owned(),
            kind: source.kind,
            jsx: None,
            text: module_loader::admit_source(account, url, source.text)
                .expect("admitted genuine module text"),
        },
    )
    .expect("actual module graph compilation")
}

fn recover(runtime: &mut Runtime, realm: Option<RuntimeRealmId>) -> String {
    let source = SourceInput::from_javascript(
        "typeof dynamicModuleResourceEffect + ':' + typeof dynamicModuleResourceCatch + ':' + typeof dynamicModuleResourceFinally",
    );
    let result = match realm {
        None => runtime.run_script(source, "dynamic-resource-recovery.js"),
        Some(realm) => runtime.run_script_in_realm(realm, source, "dynamic-resource-recovery.js"),
    }
    .expect("later explicit host turn after resource lease release");
    result.completion_string().to_owned()
}

#[test]
fn completed_dynamic_module_keeps_exact_source_refusal_in_default_and_additional_realms() {
    const LIMIT: u64 = 16 * 1024 * 1024;
    const BODY: &str = "globalThis.dynamicModuleResourceEffect = 'must not commit';";
    for additional in [false, true] {
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
                    "fillDynamicSourceAccount",
                    0,
                    RuntimeNativeCall::Dynamic(Arc::new(move |_ctx, _args, _state| {
                        let before = charge(&account);
                        let remaining = LIMIT
                            .checked_sub(before)
                            .ok_or(NativeError::InvalidOperand)?;
                        let lease = account
                            .reserve_exact(ResourceClass::SourceModuleBytes, remaining)
                            .map_err(|error| NativeError::Resource { error })?;
                        let after = charge(&account);
                        let mut pressure =
                            pressure.lock().map_err(|_| NativeError::InvalidOperand)?;
                        pressure.calls = pressure.calls.saturating_add(1);
                        pressure.observed = Some((before, after));
                        pressure.lease = Some(lease);
                        Ok(crate::Value::undefined())
                    })),
                )?;
                Ok(())
            }))
            .build()
            .expect("actual source-accounted runtime");
        let realm = additional.then(|| runtime.create_realm().expect("actual additional realm"));
        let source = format!(
            "fillDynamicSourceAccount(); try {{ eval({BODY:?}); }} catch (error) {{ globalThis.dynamicModuleResourceCatch = true; }} finally {{ globalThis.dynamicModuleResourceFinally = true; }}"
        );
        let url = "file:///dynamic-resource-module.js";
        let linked = linked(&runtime, &account, &source, url);
        let result = match realm {
            None => runtime.evaluate_dynamic_linked_module(url, linked),
            Some(realm) => {
                let records = &mut runtime.module_records;
                let config = &runtime.config;
                let task_spawner = runtime.runtime_task_spawner.clone();
                runtime
                    .interp
                    .with_host_realm(realm.realm, |interp| {
                        Ok(crate::evaluate_dynamic_linked_module_on(
                            interp,
                            records,
                            config,
                            task_spawner,
                            url,
                            linked,
                        ))
                    })
                    .expect("real traced realm extent")
            }
        };
        let error = match result {
            Err(DynLoadError::Fatal(OtterError::Resource { error })) => error,
            _ => panic!("completed actual source refusal must retain the resource boundary"),
        };
        assert_eq!(error.class(), ResourceClass::SourceModuleBytes);
        assert_eq!(error.requested(), BODY.len() as u64);
        assert_eq!(error.in_use(), Some(LIMIT));
        assert_eq!(error.limit(), Some(LIMIT));
        let mut observation = pressure.lock().unwrap();
        assert_eq!(
            observation.calls, 1,
            "one actual module body/native invocation"
        );
        let (before, after) = observation
            .observed
            .expect("actual pre-eval native ran once");
        assert!(before < LIMIT);
        assert_eq!(after, LIMIT);
        drop(observation.lease.take());
        drop(observation);
        assert_eq!(
            recover(&mut runtime, realm),
            "undefined:undefined:undefined"
        );
        drop(runtime);
        assert_eq!(
            charge(&account),
            0,
            "last module/code/source owners release their leases"
        );
    }
}
