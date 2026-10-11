//! Worker message category and accounting for escaped Array prototypes.
//!
//! # Contents
//! - Public worker messages exercise both native payload producers.
//! - Exact worker producer/consumer round trips after realm disposal and GC.
//! - Pre-admission cycle rejection through the enduring Array category.
//!
//! # Invariants
//! - Setup executes linked JavaScript; native mutations retain its source context.
//! - Materialization uses the existing native handle-scope boundary.
//! - Measurement and clone are noncollecting reads of the same retained source.
//! - Source-realm disposal cannot change an already born object's category.
//! - Awaited worker payloads are read from the owning realm after delivery;
//!   the async script entry's completion is not the message assertion.
//!
//! # See also
//! - `super::measure_worker_message` owns pre-admission accounting.
//! - `super::clone_worker_value` owns the sendable payload producer.

use super::*;
use crate::Otter;
use otter_vm::{ExecutionContext, Interpreter, NativeCallInfo};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_worker_messages_preserve_both_array_prototype_payloads() {
    let directory = tempfile::tempdir().unwrap();
    let worker_path = directory.path().join("array-prototype-worker.js");
    std::fs::write(
        &worker_path,
        r#"
        globalThis.onmessage = (event) => {
            const value = event.data;
            if (!Array.isArray(value) || value.length !== 2 || value[0] !== 7 || value[1] !== undefined) {
                throw "parent Array prototype lost its category";
            }
            Array.prototype[0] = 9;
            Array.prototype.length = 2;
            postMessage(Array.prototype);
        };
        "#,
    )
    .unwrap();
    let source = format!(
        r#"
        globalThis.workerArrayPrototypeRoundTrip = await new Promise((resolve, reject) => {{
            const worker = new Worker({:?});
            worker.onerror = (event) => {{
                worker.terminate();
                reject(new Error(event.message));
            }};
            worker.onmessage = (event) => {{
                const value = event.data;
                worker.terminate();
                if (!Array.isArray(value) || value.length !== 2 || value[0] !== 9 || value[1] !== undefined) {{
                    reject(new Error("worker Array prototype lost its category"));
                }} else {{
                    resolve("array-prototype-message");
                }}
            }};
            Array.prototype[0] = 7;
            Array.prototype.length = 2;
            worker.postMessage(Array.prototype);
        }});
        "#,
        worker_path.to_string_lossy()
    );
    let otter = Otter::builder()
        .capabilities(CapabilitySet::allow_all())
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), otter.run_script(&source))
        .await
        .expect("worker message completion")
        .expect("worker prototype round trip");
    let result = otter
        .eval("globalThis.workerArrayPrototypeRoundTrip")
        .await
        .expect("read delivered worker prototype payload");
    assert_eq!(result.completion_string(), "array-prototype-message");
}

fn link_fixture(vm: &mut Interpreter, source: &str, url: &str) -> ExecutionContext {
    let compiled = otter_compiler::compile_script_source_to_module(
        source,
        otter_syntax::SourceKind::JavaScript,
        url,
    )
    .expect("compile worker payload fixture");
    let account = vm.resource_account();
    let text = otter_resource::SharedSource::admit(&account, source.to_owned())
        .expect("worker payload fixture source");
    let sources = otter_vm::source_registry::SourceRegistry::new(
        std::collections::BTreeMap::from([(url.to_owned(), text)]),
        &account,
    )
    .expect("worker payload fixture source index");
    vm.link_module(compiled.bytecode, sources)
        .expect("link worker payload fixture")
}

fn retained_array_prototype(vm: &mut Interpreter) -> PersistentRootId {
    let realm = vm.create_host_realm().expect("source realm");
    let root = vm
        .with_host_realm(realm, |vm| {
            let context = link_fixture(
                vm,
                "Object.prototype[1] = 99; Array.prototype[0] = 7; Array.prototype.length = 2; Array.prototype;",
                "<worker-array-prototype-source>",
            );
            let prototype = vm.run(&context).expect("execute worker payload fixture");
            Ok(vm.persistent_root_insert(prototype))
        })
        .expect("retain prototype");
    assert!(vm.dispose_host_realm(realm));
    vm.force_gc().expect("collect disposed source realm");
    root
}

#[test]
fn disposed_array_prototype_round_trips_through_worker_message_payload() {
    let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
    let root = retained_array_prototype(&mut vm);
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        let value = ctx.persistent_root_get(root).expect("retained prototype");
        let transfers = WorkerTransferList::default();
        assert_eq!(
            measure_worker_message(&value, ctx, &transfers).expect("message measure"),
            3 * WORKER_MESSAGE_NODE_BYTES
        );
        let payload = clone_worker_value(&value, ctx, &transfers).expect("message clone");
        let WorkerPayload::Array(values) = &payload else {
            panic!("canonical Array prototype must produce an Array payload")
        };
        assert_eq!(values.len(), 2);
        assert!(matches!(&values[0], WorkerPayload::Number(number) if number.as_f64() == 7.0));
        assert!(matches!(&values[1], WorkerPayload::Undefined));
        ctx.scope(|mut scope| {
            let result = materialize_worker_payload_in_scope(&mut scope, &payload)
                .expect("message materialization");
            assert!(scope.is_exact_array(result));
            assert_eq!(scope.array_length(result).unwrap(), 2);
            let first = scope.index(result, 0).unwrap();
            let hole = scope.index(result, 1).unwrap();
            assert_eq!(scope.number_value(first).unwrap(), 7.0);
            assert!(scope.is_undefined(hole));
        });
    });
    vm.persistent_root_remove(root).expect("release prototype");
}

#[test]
fn disposed_array_prototype_cycle_is_rejected_before_message_admission() {
    let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
    let root = retained_array_prototype(&mut vm);
    let context = link_fixture(&mut vm, "undefined;", "<worker-array-cycle-source>");
    NativeCtx::with_host_context(
        &mut vm,
        NativeCallInfo::default_call(),
        Some(&context),
        |ctx| {
            let value = ctx.persistent_root_get(root).expect("retained prototype");
            ctx.scope(|mut scope| {
                let source = scope.value(value);
                scope.set(source, "0", source).expect("cyclic indexed data");
            });
            let value = ctx.persistent_root_get(root).expect("retained cycle");
            let transfers = WorkerTransferList::default();
            for result in [
                measure_worker_message(&value, ctx, &transfers).map(|_| ()),
                clone_worker_value(&value, ctx, &transfers).map(|_| ()),
            ] {
                assert!(matches!(
                    result,
                    Err(NativeError::TypeError { reason, .. })
                        if reason == "cycle detected at $[0]"
                ));
            }
        },
    );
    vm.persistent_root_remove(root).expect("release prototype");
}
