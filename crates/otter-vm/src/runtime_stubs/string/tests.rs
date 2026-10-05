//! Typed primitive operation roots, exact content ordering and refusal proofs.
//!
//! # Contents
//! - Real published allocating frames and moving string inputs at strides 1..16.
//! - Rope/slice/non-ASCII/surrogate order and numeric primitive edge cases.
//! - Fixed registry ownership and unsupported-tag refusal before effects.
//!
//! # Invariants
//! Every fixture value is built in the VM's production handle arena. The tested
//! call retains inputs only through its declared safepoint/ABI roots. Callback
//! observations own no moving handle after their synchronous extent.
//! Pure probes preserve managed allocation/collection/cache state exactly.
//!
//! # See also
//! - `super` owns the current primitive VM entries.

use super::*;
use crate::{
    ExecutionContext, Frame,
    native_abi::{
        CodeRegistryView, NO_FRAME_STATE, NativeFrameFlags, NativeFrameKind, NativeResultStatus,
        VmFrameHeader, VmThread,
    },
};
use otter_bytecode::{FunctionCodeBuilder, Op, Operand};

fn source() -> ExecutionContext {
    let mut module = crate::test_support::minimal_bytecode_module("typed-string.js");
    let mut code = FunctionCodeBuilder::new();
    code.push(
        Op::Add,
        &[
            Operand::Register(0),
            Operand::Register(1),
            Operand::Register(2),
        ],
    );
    code.push(Op::Return, &[Operand::Register(0)]);
    module.functions[0].code = code.finish();
    module.functions[0].locals = 5;
    ExecutionContext::from_module(module, crate::source_registry::SourceRegistry::default())
        .unwrap()
}
unsafe extern "C" fn resolve(context: u64, code: u64, id: SafepointId) -> *const SafepointRecord {
    // SAFETY: the immutable local map is published for this synchronous fixture.
    let record = unsafe { &*(context as *const SafepointRecord) };
    if code == 1 && record.id == id {
        record
    } else {
        std::ptr::null()
    }
}
fn call_fixture(
    vm: &mut Interpreter,
    mut slots: [Value; 5],
    action: impl FnOnce(&mut Interpreter, &mut RuntimeStubAllocContext, &mut [Value; 5]),
) {
    let source = source();
    let mut frame = Frame::new(
        VmFrameHeader {
            function_id: 0,
            pc: 0,
            register_count: 5,
            kind: NativeFrameKind::Baseline,
            flags: NativeFrameFlags::from_bits(NativeFrameFlags::HAS_SAFEPOINTS),
        },
        slots.as_mut_ptr() as u64,
        Value::function(0),
        Value::undefined(),
    );
    frame.code_object_id = 1;
    // Production publishes the calling frame; the collector traces its
    // canonical window, which no safepoint record names.
    // SAFETY: the frame and its slot window outlive the action and are
    // unpublished below before either drops.
    unsafe { vm.jit_push_native_frame(&mut frame) }.expect("publish fixture frame");
    let mut frame_cell = std::ptr::from_mut(&mut frame) as u64;
    let record = SafepointRecord::window(41, NO_FRAME_STATE);
    let registry = CodeRegistryView {
        context: std::ptr::from_ref(&record) as u64,
        resolve_safepoint: resolve as *const () as u64,
        function_entries: 0,
        function_entry_count: 0,
        // This fixture performs a direct allocating C entry and has no JS child.
        resolve_return_pc: 0,
    };
    let mut stack = crate::test_support::FrameChainFixture::new();
    let activation = crate::jit::VmRuntimeActivation::new(vm, &mut stack, Some(&source));
    let mut thread = VmThread::empty();
    thread.frame_cell = std::ptr::from_mut(&mut frame_cell) as u64;
    thread.runtime_context = std::ptr::from_ref(&activation) as u64;
    thread.code_registry = std::ptr::from_ref(&registry) as u64;
    let mut packet = RuntimeStubAllocContext::new(&mut thread, record.id);
    action(vm, &mut packet, &mut slots);
    vm.jit_pop_native_frame();
}

#[test]
fn current_registry_resolves_the_one_pair_and_validates_both_new_shapes() {
    assert_eq!(
        alloc_value_stub_by_id(crate::native_abi::STUB_ALLOC_GROUP_ENSURE.id)
            .unwrap()
            .entry_addr(),
        ALLOC_GROUP_ENSURE.entry_addr()
    );
    assert_eq!(
        leaf_no_alloc_stub2_by_id(crate::native_abi::STUB_PRIMITIVE_STRING_ORDER.id)
            .unwrap()
            .entry_addr(),
        PRIMITIVE_STRING_ORDER.entry_addr()
    );
    assert!(PRIMITIVE_STRING_ORDER.is_valid());
    assert!(!ALLOC_GROUP_ENSURE.is_valid_for_safepoint(NO_SAFEPOINT));
    assert!(ALLOC_GROUP_ENSURE.is_valid_for_safepoint(41));
}

#[test]
fn scoped_concat_rewrites_all_published_aliases_inside_the_actual_call_at_every_stride() {
    for stride in 1..=16 {
        let mut vm = Interpreter::new().expect("typed string interpreter");
        vm.gc_heap.set_gc_stress(0, false);
        let values = vm.with_handle_scope(|vm, scope| {
            let a = crate::JsString::from_latin1(b"long-left-abcdefghijklmnop", &mut vm.gc_heap)
                .unwrap();
            let a = vm.scoped_value(scope, Value::string(a));
            let b = crate::JsString::from_utf16_units(
                &[
                    0x100, 0xd800, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e,
                ],
                &mut vm.gc_heap,
            )
            .unwrap();
            [vm.escape_scoped(a), Value::string(b)]
        });
        let mut expected = values[0]
            .as_string(&vm.gc_heap)
            .unwrap()
            .to_utf16_vec(&vm.gc_heap);
        expected.extend(
            values[1]
                .as_string(&vm.gc_heap)
                .unwrap()
                .to_utf16_vec(&vm.gc_heap),
        );
        let originals = values.map(|value| value.to_abi_bits());
        vm.gc_heap.set_gc_stress(stride, false);
        for _ in 1..stride {
            vm.gc_heap
                .alloc(otter_gc::test_support::OpaqueLeaf { payload: 0 })
                .unwrap();
        }
        let before = vm.gc_heap.gc_stats().clone();
        call_fixture(
            &mut vm,
            [
                Value::undefined(),
                values[0],
                values[1],
                values[0],
                values[1],
            ],
            |vm, packet, slots| {
                let pair = string_concat_alloc(
                    packet,
                    41,
                    slots[1].to_abi_bits(),
                    slots[2].to_abi_bits(),
                    Value::undefined().to_abi_bits(),
                );
                assert_eq!(
                    pair.validate(NativeResultDomain::Probe),
                    Some(NativeResultStatus::Success)
                );
                let result = Value::from_abi_bits(pair.payload_bits());
                assert_eq!(
                    result
                        .as_string(&vm.gc_heap)
                        .unwrap()
                        .to_utf16_vec(&vm.gc_heap),
                    expected
                );
                assert!(vm.gc_heap.gc_stats().minor_gc_cycles > before.minor_gc_cycles);
                assert!(vm.gc_heap.gc_stats().minor_slot_updates > before.minor_slot_updates);
                assert_ne!(slots[1].to_abi_bits(), originals[0]);
                assert_ne!(slots[2].to_abi_bits(), originals[1]);
                assert_eq!(slots[1], slots[3]);
                assert_eq!(slots[2], slots[4]);
                vm.with_handle_scope(|vm, scope| {
                    let result = vm.scoped_value(scope, result);
                    vm.gc_heap.set_gc_stress(0, false);
                    vm.gc_heap.collect_full(&mut |_| {}).unwrap();
                    let result = vm.escape_scoped(result).as_string(&vm.gc_heap).unwrap();
                    assert_eq!(result.to_utf16_vec(&vm.gc_heap), expected);
                });
            },
        );
    }
}

#[test]
fn pure_order_handles_rope_slice_surrogates_numeric_text_and_nan_without_heap_effects() {
    let mut vm = Interpreter::new().expect("typed order interpreter");
    vm.gc_heap.set_gc_stress(1, false);
    vm.with_handle_scope(|vm, scope| {
        let mut strings = Vec::new();
        for text in [
            "abcdefghijklmnopqrstuvwxy",
            "z",
            "",
            "-0",
            "NaN",
            "Infinity",
            "0x10",
        ] {
            let value = crate::JsString::from_str(text, &mut vm.gc_heap).unwrap();
            strings.push(vm.scoped_value(scope, Value::string(value)));
        }
        for units in [&[0xd800][..], &[0xe000][..]] {
            let value = crate::JsString::from_utf16_units(units, &mut vm.gc_heap).unwrap();
            strings.push(vm.scoped_value(scope, Value::string(value)));
        }
        let a = vm.escape_scoped(strings[0]).as_string(&vm.gc_heap).unwrap();
        let b = vm.escape_scoped(strings[1]).as_string(&vm.gc_heap).unwrap();
        let rope = crate::JsString::concat(a, b, &mut vm.gc_heap).unwrap();
        let rope = vm.scoped_value(scope, Value::string(rope));
        let slice = vm
            .escape_scoped(rope)
            .as_string(&vm.gc_heap)
            .unwrap()
            .slice(1, 24, &mut vm.gc_heap)
            .unwrap();
        let slice = vm.scoped_value(scope, Value::string(slice));
        let get = |vm: &Interpreter, index| vm.escape_scoped(strings[index]);
        let cases = [
            (get(vm, 0), get(vm, 1), -1),
            (vm.escape_scoped(rope), get(vm, 0), 1),
            (vm.escape_scoped(slice), get(vm, 0), 1),
            (get(vm, 7), get(vm, 8), -1),
            (get(vm, 2), Value::number_i32(0), 0),
            (get(vm, 3), Value::number_f64(-0.0), 0),
            (get(vm, 4), Value::number_i32(0), 2),
            (Value::number_f64(f64::NAN), get(vm, 2), 2),
            (get(vm, 5), Value::number_f64(f64::INFINITY), 0),
            (get(vm, 6), Value::number_i32(16), 0),
        ];
        let before = vm.gc_heap.gc_stats().clone();
        for (a, b, expected) in cases {
            let pair = primitive_string_order(&vm.gc_heap, a.to_abi_bits(), b.to_abi_bits());
            assert_eq!(
                pair.validate(NativeResultDomain::Probe),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(
                Value::from_abi_bits(pair.payload_bits()).as_i32(),
                Some(expected)
            );
        }
        for invalid in [Value::undefined(), Value::null(), Value::boolean(false)] {
            let pair = primitive_string_order(
                &vm.gc_heap,
                get(vm, 0).to_abi_bits(),
                invalid.to_abi_bits(),
            );
            assert_eq!(
                pair.validate(NativeResultDomain::Probe),
                Some(NativeResultStatus::SideExit)
            );
        }
        let after = vm.gc_heap.gc_stats().clone();
        assert_eq!(after.alloc_bytes_total, before.alloc_bytes_total);
        assert_eq!(after.minor_gc_cycles, before.minor_gc_cycles);
        assert_eq!(after.gc_cycles, before.gc_cycles);
    });
}

#[test]
fn unsupported_concat_and_disabled_group_miss_before_allocating_or_latching_oom() {
    let mut vm = Interpreter::new().expect("typed refusal interpreter");
    vm.gc_heap.set_gc_stress(1, false);
    let before = vm.gc_heap.gc_stats().clone();
    call_fixture(&mut vm, [Value::undefined(); 5], |vm, packet, _| {
        for operands in [
            [Value::boolean(true), Value::number_i32(2)],
            [Value::number_i32(1), Value::number_i32(2)],
        ] {
            let pair = string_concat_alloc(
                packet,
                41,
                operands[0].to_abi_bits(),
                operands[1].to_abi_bits(),
                Value::undefined().to_abi_bits(),
            );
            assert_eq!(
                pair.validate(NativeResultDomain::Probe),
                Some(NativeResultStatus::SideExit)
            );
        }
        let pair = alloc_group_ensure(
            packet,
            41,
            Value::number_i32(128).to_abi_bits(),
            Value::undefined().to_abi_bits(),
            Value::undefined().to_abi_bits(),
        );
        assert_eq!(
            pair.validate(NativeResultDomain::Probe),
            Some(NativeResultStatus::SideExit)
        );
        let after = vm.gc_heap.gc_stats().clone();
        assert_eq!(after.alloc_bytes_total, before.alloc_bytes_total);
        assert_eq!(after.minor_gc_cycles, before.minor_gc_cycles);
        assert!(
            !vm.gc_heap
                .oom_flag()
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    });
}
