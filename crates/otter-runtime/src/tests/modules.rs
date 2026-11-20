//! Module loading and import admission.
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

#[test]
fn looks_like_module_detects_import_at_top_level() {
    let text = "import { x } from \"./y.ts\";\n";
    assert!(source_text_looks_like_module(text, SourceKind::TypeScript));
}

#[test]
fn looks_like_module_detects_export_function() {
    let text = "export function f() {}\n";
    assert!(source_text_looks_like_module(text, SourceKind::TypeScript));
}

#[test]
fn looks_like_module_with_leading_block_comment() {
    let text = "/* hi */\nimport { x } from \"./y.ts\";\n";
    assert!(source_text_looks_like_module(text, SourceKind::TypeScript));
}

#[test]
fn module_program_runs_two_file_static_import() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("other.ts"), "export let value = 7;\n").unwrap();
    std::fs::write(
            dir.path().join("entry.ts"),
            "import { value } from \"./other.ts\";\nfunction fail() { return undefined.x; }\nif (value !== 7) fail();\n",
        )
        .unwrap();
    let otter = Otter::new();
    otter
        .blocking_run_file(dir.path().join("entry.ts"))
        .unwrap();
}

#[test]
fn module_program_imports_json_default() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.json"), r#"{"answer":42}"#).unwrap();
    std::fs::write(
            dir.path().join("entry.ts"),
            "import data from \"./data.json\";\nfunction fail() { return undefined.x; }\nif (data.answer !== 42) fail();\n",
        )
        .unwrap();

    Otter::new()
        .blocking_run_file(dir.path().join("entry.ts"))
        .unwrap();
}

#[test]
fn module_program_imports_json_with_type_attribute() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.json"), r#"{"answer":42}"#).unwrap();
    std::fs::write(
            dir.path().join("entry.ts"),
            "import data from \"./data.json\" with { type: \"json\" };\nfunction fail() { return undefined.x; }\nif (data.answer !== 42) fail();\n",
        )
        .unwrap();

    Otter::new()
        .blocking_run_file(dir.path().join("entry.ts"))
        .unwrap();
}

#[test]
fn module_program_propagates_live_binding_writes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("counter.ts"),
            "export let count = 0;\nexport function inc() { count = count + 1; }\nexport function get() { return count; }\n",
        )
        .unwrap();
    std::fs::write(
            dir.path().join("entry.ts"),
            "import { inc, get } from \"./counter.ts\";\nfunction fail() { return undefined.x; }\ninc(); inc();\nif (get() !== 2) fail();\n",
        )
        .unwrap();
    let otter = Otter::new();
    otter
        .blocking_run_file(dir.path().join("entry.ts"))
        .unwrap();
}

/// ECMA-262 §16.2.1 Cyclic Module Records — the loader must
/// short-circuit cyclic edges and rely on live-binding
/// indirection through `module_env`. Both modules must
/// compile and evaluate; spec correctness checks live in
/// `tests/module_cycle_and_lifecycle.rs`.
#[test]
fn module_program_accepts_cycle_per_spec() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.ts"),
        "import { b } from \"./b.ts\";\nexport let a = 1;\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.ts"),
        "import { a } from \"./a.ts\";\nexport let b = 2;\n",
    )
    .unwrap();
    let otter = Otter::new();
    otter
        .blocking_run_file(dir.path().join("a.ts"))
        .expect("two-file cycle must run per §16.2.1");
}

#[test]
fn module_program_dynamic_import_literal_resolves_to_namespace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("util.ts"), "export let answer = 42;\n").unwrap();
    std::fs::write(
            dir.path().join("entry.ts"),
            "function fail() { return undefined.x; }\nimport(\"./util.ts\").then((m) => { if (m.answer !== 42) fail(); });\n",
        )
        .unwrap();
    let otter = Otter::new();
    otter
        .blocking_run_file(dir.path().join("entry.ts"))
        .unwrap();
}

#[test]
fn module_program_dynamic_import_invalid_target_rejects_syntax_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("bad.js"),
        "var smoosh; function smoosh() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("entry.mjs"),
        "function fail() { return undefined.x; }\n\
             import(\"./bad.js\")\n\
               .catch((error) => { if (error.name !== \"SyntaxError\") fail(); })\n\
               .then(() => {}, fail);\n",
    )
    .unwrap();

    let mut runtime = Runtime::builder().build().unwrap();
    runtime.run_module(dir.path().join("entry.mjs")).unwrap();
}

#[test]
fn layer_a_dynamic_import_capacity_rejects_before_registering_pending_work() {
    let mut runtime = Runtime::builder()
        .completion_capacities(0, 1, 1)
        .build()
        .expect("zero guaranteed capacity is a valid fail-closed runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
                    globalThis.dynamicAdmission = "pending";
                    const target = "./never-loaded.js";
                    import(target).then(
                        () => { globalThis.dynamicAdmission = "unexpected-success"; },
                        (error) => {
                            globalThis.dynamicAdmission =
                                error.name + ":" + /capacity/.test(error.message);
                        },
                    );
                    "#,
            ),
            "<dynamic-admission-exhausted>",
        )
        .expect("dynamic import admission rejection is delivered as a Promise rejection");
    let result = runtime
        .eval(SourceInput::from_javascript("globalThis.dynamicAdmission;"))
        .expect("read rejection outcome");

    assert_eq!(result.completion_string(), "TypeError:true");
    assert!(!runtime.has_pending_work());
    let snapshot = runtime.resource_snapshot();
    assert_eq!(snapshot.get(ResourceClass::QueuedTasks).current(), 0);
    assert_eq!(snapshot.get(ResourceClass::HostOperations).current(), 0);
}

#[test]
fn excessive_completion_capacities_return_config_errors_without_panicking_or_admission() {
    let excessive = tokio::sync::Semaphore::MAX_PERMITS
        .checked_add(1)
        .expect("Tokio semaphore maximum leaves one invalid usize value");

    for capacities in [(excessive, 1, 1), (1, excessive, 1), (1, 1, excessive)] {
        let account = ResourceAccount::default();
        let build = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let account = account.clone();
            move || {
                Runtime::builder()
                    .resource_account(account)
                    .completion_capacities(capacities.0, capacities.1, capacities.2)
                    .build()
            }
        }));

        let error = build
            .expect("invalid public capacity must not panic")
            .expect_err("invalid public capacity must be rejected");
        assert!(matches!(
            error,
            OtterError::Config {
                reason: ConfigError::InvalidCompletionCapacity { .. }
            }
        ));
        assert_eq!(
            account.snapshot().get(ResourceClass::Isolates).current(),
            0,
            "validation precedes runtime-role admission"
        );
    }
}

#[test]
fn layer_a_rejects_foreign_admission_carriers_without_retention() {
    let capacities = completion_admission::CompletionCapacities {
        guaranteed: 1,
        host_operations: 1,
        timers: 1,
    };
    let account_a = ResourceAccount::default();
    let account_b = ResourceAccount::default();
    let queue_a = LayerADynamicImportQueue::default();
    let queue_b = LayerADynamicImportQueue::default();
    let loader_a = LayerADynamicImportLoader {
        queue: queue_a.clone(),
        completion_pool: completion_admission::CompletionAdmissionPool::new(
            account_a.clone(),
            capacities,
        ),
    };
    let loader_b = LayerADynamicImportLoader {
        queue: queue_b.clone(),
        completion_pool: completion_admission::CompletionAdmissionPool::new(
            account_b.clone(),
            capacities,
        ),
    };

    let admission =
        otter_vm::DynamicImportLoader::admit(&loader_a).expect("first-runtime admission");
    assert!(
        otter_vm::DynamicImportLoader::schedule(
            &loader_b,
            admission,
            1,
            "x".to_string(),
            String::new(),
            None,
        )
        .is_err()
    );
    assert!(
        otter_vm::DynamicImportLoader::schedule(
            &loader_b,
            otter_vm::DynamicImportAdmission::new(Box::new(())),
            2,
            "x".to_string(),
            String::new(),
            None,
        )
        .is_err()
    );

    assert!(queue_a.lock().expect("first queue").is_empty());
    assert!(queue_b.lock().expect("second queue").is_empty());
    for account in [account_a, account_b] {
        let snapshot = account.snapshot();
        assert_eq!(snapshot.get(ResourceClass::QueuedTasks).current(), 0);
        assert_eq!(snapshot.get(ResourceClass::HostOperations).current(), 0);
    }
}

#[test]
fn layer_a_fatal_cleanup_removes_current_and_queued_import_roots() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let (_, context) = runtime
        .run_script_with_context(SourceInput::from_javascript("0"), "<dynamic-cleanup>")
        .expect("establish execution context");
    let current_promise =
        otter_vm::promise_dispatch::pending_runtime_rooted(&mut runtime.interp, &mut [], &mut [])
            .expect("current pending promise");
    let current_token =
        runtime
            .interp
            .dynamic_import_registry_mut()
            .insert(current_promise, context.clone(), 0);
    let queued_promise =
        otter_vm::promise_dispatch::pending_runtime_rooted(&mut runtime.interp, &mut [], &mut [])
            .expect("queued pending promise");
    let queued_token =
        runtime
            .interp
            .dynamic_import_registry_mut()
            .insert(queued_promise, context, 0);

    let account = ResourceAccount::default();
    let pool = completion_admission::CompletionAdmissionPool::new(
        account.clone(),
        completion_admission::CompletionCapacities {
            guaranteed: 2,
            host_operations: 2,
            timers: 1,
        },
    );
    let current = pool
        .admit(completion_admission::CompletionOrigin::HostOperation)
        .expect("current admission")
        .begin_dispatch();
    let queued = pool
        .admit(completion_admission::CompletionOrigin::HostOperation)
        .expect("queued admission");
    runtime
        .layer_a_dynamic_imports
        .lock()
        .expect("Layer A queue")
        .push_back(LayerADynamicImportRequest {
            admission: queued,
            token: queued_token,
            specifier: "unused".to_string(),
            referrer: String::new(),
            attr_type: None,
        });

    runtime.cancel_layer_a_dynamic_imports_after_failure(current_token);
    drop(current);

    assert!(runtime.interp.dynamic_import_registry().is_empty());
    assert!(
        runtime
            .layer_a_dynamic_imports
            .lock()
            .expect("Layer A queue")
            .is_empty()
    );
    let snapshot = account.snapshot();
    assert_eq!(snapshot.get(ResourceClass::QueuedTasks).current(), 0);
    assert_eq!(snapshot.get(ResourceClass::HostOperations).current(), 0);
}

#[test]
fn module_program_import_meta_url_matches_canonical() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("entry.ts"),
            "function fail() { return undefined.x; }\nlet u = import.meta.url;\nif (u.indexOf(\"file://\") !== 0) fail();\nif (u.indexOf(\"entry.ts\") < 0) fail();\n",
        )
        .unwrap();
    let otter = Otter::new();
    otter
        .blocking_run_file(dir.path().join("entry.ts"))
        .unwrap();
}
