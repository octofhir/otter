//! Actual JSON allocation refusals and selected-prototype relocation.
//!
//! # Contents
//! - Parse, rawJSON and stringify preserve native OutOfMemory identity.
//! - Malformed source remains a syntax error even at a full cap.
//! - Container construction rereads one traced prototype after collection.
//!
//! # Invariants
//! - Inputs use the existing NativeCtx handle scope and production collector.
//! - Pressure admission settles retained bytes before the observed phase.
//!   The tested operation causes every claimed cap-triggered pause.
//! - The original allocator failure reaches the existing native error DTO.
//!
//! # See also
//! - `super::native_json_error` owns the exhaustive native projection.
//! - `super::parse::parse_with_roots` owns the canonical pending prototype.

use super::*;
use crate::{Interpreter, NativeCallInfo};

struct Funding([u64; 16384]);
impl otter_gc::SafeTraceable for Funding {
    const TYPE_TAG: u8 = 0xe5;
    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        let _ = self.0[0];
    }
}

#[test]
fn actual_cap_refusals_remain_native_oom_for_parse_raw_and_stringify() {
    use otter_bytecode::method_id::JsonMethod;
    for (name, source, method) in [
        (
            "parse",
            "{\"field\":\"a fresh parsed payload\"}",
            Some(JsonMethod::Parse),
        ),
        ("rawJSON", "\"a fresh parsed payload\"", None),
        (
            "stringify",
            "a fresh stringify payload",
            Some(JsonMethod::Stringify),
        ),
    ] {
        let cap = 4 * 1024 * 1024;
        let mut vm = Interpreter::with_string_heap_cap(cap).expect("JSON fixture bootstrap");
        vm.gc_heap.set_gc_stress(0, false);
        let context = vm
            .link_module(
                crate::test_support::minimal_bytecode_module("json-cap"),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        NativeCtx::with_host_context(
            &mut vm,
            NativeCallInfo::default_call(),
            Some(&context),
            |ctx| {
                ctx.scope(|mut scope| {
                    let input = scope.string(source).expect("native source input");
                    scope
                        .context()
                        .interp_mut()
                        .force_gc()
                        .expect("settle inputs and bootstrap");
                    let reserve = cap - scope.context().heap().stats().allocated_bytes as u64;
                    scope
                        .context()
                        .heap_mut()
                        .reserve_bytes_with_roots(reserve, &mut |_| {})
                        .expect("admit full live cap before observation");
                    let before = scope.context().heap().gc_cycle_counts();
                    let args = [scope.raw(input)];
                    let result = if let Some(method) = method {
                        native_json_call(scope.context(), method, &args, None)
                    } else {
                        native_raw_json(scope.context(), &args)
                    };
                    scope.context().heap_mut().release_bytes(reserve);
                    assert!(scope.context().heap().gc_cycle_counts().1 > before.1);
                    match result.expect_err("actual allocator refusal must retain its identity") {
                        NativeError::OutOfMemory {
                            name: actual,
                            requested_bytes,
                            heap_limit_bytes,
                        } => {
                            assert_eq!(actual, name);
                            assert!(requested_bytes > 0);
                            assert_eq!(heap_limit_bytes, cap);
                        }
                        other => panic!("expected native OutOfMemory for {name}, got {other:?}"),
                    }
                    assert_eq!(scope.context().heap().stats().reserved_bytes, 0);
                    let input = scope.raw(input);
                    let heap = scope.context().heap();
                    assert_eq!(input.as_string(heap).unwrap().to_lossy_string(heap), source);
                });
            },
        );
    }
}

#[test]
fn malformed_source_at_full_cap_remains_syntax_without_an_allocation_pause() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("JSON fixture bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("json-syntax-cap"),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
    NativeCtx::with_host_context(
        &mut vm,
        NativeCallInfo::default_call(),
        Some(&context),
        |ctx| {
            ctx.scope(|mut scope| {
                let input = scope.string("[1,]").unwrap();
                scope
                    .context()
                    .interp_mut()
                    .force_gc()
                    .expect("settle inputs");
                let reserve = cap - scope.context().heap().stats().allocated_bytes as u64;
                scope
                    .context()
                    .heap_mut()
                    .reserve_bytes_with_roots(reserve, &mut |_| {})
                    .unwrap();
                let before = scope.context().heap().gc_cycle_counts();
                let args = [scope.raw(input)];
                let result = native_json_call(
                    scope.context(),
                    otter_bytecode::method_id::JsonMethod::Parse,
                    &args,
                    None,
                );
                scope.context().heap_mut().release_bytes(reserve);
                assert_eq!(scope.context().heap().gc_cycle_counts(), before);
                assert!(
                    matches!(result, Err(NativeError::SyntaxError { name: "parse", reason })
                if reason.contains("trailing comma") && reason.contains("byte 3"))
                );
            });
        },
    );
}

#[test]
fn parse_reloads_selected_prototype_and_its_child_after_actual_cap_collection() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("JSON fixture bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let prototype = scope.object().unwrap();
            let child = scope.object().unwrap();
            let prototype_value = scope.raw(prototype);
            let alias = scope.value(prototype_value);
            scope
                .define(
                    prototype,
                    "marker",
                    child,
                    crate::object::PropertyFlags::data_default(),
                )
                .unwrap();
            let offsets = [
                scope.raw(prototype).as_object().unwrap().offset(),
                scope.raw(child).as_object().unwrap().offset(),
            ];
            assert_ne!(offsets[0], offsets[1]);
            let before = scope.context().heap().gc_cycle_counts();
            scope
                .context()
                .heap_mut()
                .alloc_old(Funding([0; 16384]))
                .unwrap();
            let reserve = cap - scope.context().heap().tracked_bytes();
            scope
                .context()
                .heap_mut()
                .reserve_bytes_no_collect(reserve)
                .unwrap();
            assert_eq!(scope.context().heap().gc_cycle_counts(), before);
            let prototype_value = scope.raw(prototype);
            let result = parse::parse_with_roots(
                "{\"field\":\"fresh parsed payload\",\"__proto__\":7}",
                scope.context().heap_mut(),
                &mut |_| {},
                Some(prototype_value),
            );
            scope.context().heap_mut().release_bytes(reserve);
            let result = scope.value(result.expect("collecting JSON construction"));
            assert!(scope.context().heap().gc_cycle_counts().1 > before.1);
            assert_ne!(
                scope.raw(prototype).as_object().unwrap().offset(),
                offsets[0]
            );
            assert_ne!(scope.raw(child).as_object().unwrap().offset(), offsets[1]);
            assert_eq!(scope.raw(prototype), scope.raw(alias));
            let prototype = scope.raw(prototype).as_object().unwrap();
            let child = scope.raw(child);
            let object = scope.raw(result).as_object().unwrap();
            let heap = scope.context().heap();
            assert_eq!(crate::object::prototype(object, heap), Some(prototype));
            assert_eq!(
                crate::object::get_own(prototype, heap, "marker"),
                Some(child)
            );
            let descriptor = crate::object::get_own_descriptor(object, heap, "__proto__").unwrap();
            assert!(descriptor.writable() && descriptor.enumerable() && descriptor.configurable());
            assert!(
                matches!(descriptor.kind, crate::object::DescriptorKind::Data { value }
                if value == Value::number_i32(7))
            );
            let value = crate::object::get_own(object, heap, "field").unwrap();
            assert_eq!(
                value.as_string(heap).unwrap().to_lossy_string(heap),
                "fresh parsed payload"
            );
        });
    });
}

#[test]
fn installed_stringify_actual_native_call_preserves_vm_and_native_oom() {
    let cap = 4 * 1024 * 1024;
    let mut vm = Interpreter::with_string_heap_cap(cap).expect("JSON native fixture bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    let context = vm
        .link_module(
            crate::test_support::minimal_bytecode_module("json-native-cap"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("verifier-admitted actual native call context");
    NativeCtx::with_host_context(
        &mut vm,
        NativeCallInfo::default_call(),
        Some(&context),
        |ctx| {
            ctx.scope(|mut scope| {
            let json = scope.global("JSON").expect("installed JSON namespace");
            let stringify = scope.get(json, "stringify").expect("installed JSON.stringify");
            assert!(scope.is_callable(stringify));
            let input = scope.string("fresh serialized result").unwrap();
            // Resolve lazy namespace/method state before cap admission. These
            // calls use the installed native function and the real linked
            // ExecutionContext/ActivationStack, through host_call_entry.
            scope.context().interp_mut().force_gc().expect("settle live native call inputs");
            let reserve = cap - scope.context().heap().stats().allocated_bytes as u64;
            scope.context().heap_mut().reserve_bytes_with_roots(reserve, &mut |_| {})
                .expect("admit exact physical full-cap native inputs");
            let before = scope.context().heap().gc_cycle_counts();
            let error = scope.call_vm(stringify, json, &[input])
                .expect_err("installed native serializer must retain actual VM OOM");
            assert!(matches!(error, crate::VmError::OutOfMemory { requested_bytes, heap_limit_bytes }
                if requested_bytes > 0 && heap_limit_bytes == cap));
            assert!(scope.context().heap().gc_cycle_counts().1 > before.1);
            let before = scope.context().heap().gc_cycle_counts();
            let error = scope.call(stringify, json, &[input])
                .expect_err("sole NativeScope projection retains actual native OOM");
            assert!(matches!(error, NativeError::ExecutionFailure(crate::RunError {
                error: crate::VmError::OutOfMemory { requested_bytes, heap_limit_bytes }, .. })
                if requested_bytes > 0 && heap_limit_bytes == cap));
            assert!(scope.context().heap().gc_cycle_counts().1 > before.1);
            scope.context().heap_mut().release_bytes(reserve);
            assert_eq!(scope.context().heap().stats().reserved_bytes, 0);
            let input = scope.raw(input);
            let heap = scope.context().heap();
            assert_eq!(input.as_string(heap).unwrap().to_lossy_string(heap), "fresh serialized result");
        });
        },
    );
}
