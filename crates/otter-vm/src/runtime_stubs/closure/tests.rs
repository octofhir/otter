//! Real typed closure-stub source and moving input-root proofs.
//!
//! # Contents
//! - Fixed descriptor/registry ownership and distinct function evaluations.
//! - Explicit lexical inputs relocated inside the canonical allocation.
//! - Malformed source/opcode/safepoint refusal before allocation.
//!
//! # Invariants
//! - All values are built under the production handle scope; only the published
//!   frame and typed call roots retain them at the tested allocating boundary.
//! - Stress priming cannot collect; the closure allocation itself must collect.
//! - Synthetic resolver metadata is immutable and lives for the whole call.
//!
//! # See also
//! - `super` owns the one current compiled closure construction boundary.

use super::*;
use crate::{
    ExecutionContext, Frame, Interpreter,
    native_abi::{
        CodeRegistryView, NO_FRAME_STATE, NativeFrameFlags, NativeFrameKind, NativeResultStatus,
        SafepointRecord, VmFrameHeader, VmThread,
    },
};
use otter_bytecode::{
    Constant, Function, FunctionCodeBuilder, Operand, ScopeDescriptor, ScopeFlags, ScopeKind,
    SlotDescriptor, SlotKind,
};

struct StressPrime {
    _word: u64,
}
impl otter_gc::SafeTraceable for StressPrime {
    const TYPE_TAG: u8 = 0xf7;
    fn trace_slots_safe(&mut self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {}
}
fn context() -> ExecutionContext {
    let mut module = crate::test_support::minimal_bytecode_module("typed-lexical-allocation.js");
    let mut code = FunctionCodeBuilder::new();
    code.push(
        Op::MakeFunction,
        &[Operand::Register(0), Operand::ConstIndex(0)],
    );
    code.push(
        Op::MakeClosure,
        &[
            Operand::Register(1),
            Operand::ConstIndex(1),
            Operand::Register(2),
        ],
    );
    code.push(Op::ReturnUndefined, &[]);
    module.functions[0].locals = 5;
    module.functions[0].code = code.finish();
    module.functions[0].scopes = vec![ScopeDescriptor {
        kind: ScopeKind::Body,
        flags: ScopeFlags::default(),
        slots: vec![SlotDescriptor {
            name: "captured".into(),
            kind: SlotKind::Let,
            exported: false,
        }],
    }];
    for (id, arrow) in [(1, false), (2, true)] {
        let mut code = FunctionCodeBuilder::new();
        code.push(Op::ReturnUndefined, &[]);
        module.functions.push(Function {
            id,
            name: format!("target{id}"),
            is_arrow: arrow,
            code: code.finish(),
            ..Default::default()
        });
    }
    module.constants = vec![
        Constant::FunctionId { index: 1 },
        Constant::FunctionId { index: 2 },
    ];
    ExecutionContext::from_module(module, crate::source_registry::SourceRegistry::default())
        .expect("verifier-valid typed closure source")
}
unsafe extern "C" fn resolve(context: u64, code: u64, id: SafepointId) -> *const SafepointRecord {
    // SAFETY: this synchronous fixture publishes the exact local immutable record.
    let record = unsafe { &*(context as *const SafepointRecord) };
    if code == 1 && id == record.id {
        record
    } else {
        std::ptr::null()
    }
}
fn inputs(vm: &mut Interpreter, source: &ExecutionContext) -> [Value; 3] {
    vm.with_handle_scope(|vm, scope| {
        let context = vm
            .create_context_value(source, 0, 0, Value::undefined())
            .unwrap();
        assert!(crate::context::write_slot(
            &mut vm.gc_heap,
            context.as_context().unwrap(),
            0,
            Value::number_i32(404)
        ));
        let context = vm.scoped_value(scope, context);
        let this = vm.scoped_object(scope).unwrap();
        let target = vm.scoped_object(scope).unwrap();
        [
            vm.escape_scoped(context),
            vm.escape_scoped(this),
            vm.escape_scoped(target),
        ]
    })
}

fn fixture(
    operation: impl FnOnce(
        &mut Interpreter,
        &ExecutionContext,
        &mut Frame,
        &mut RuntimeStubAllocContext,
        &mut [Value; 5],
    ),
) {
    let source = context();
    let mut vm =
        Interpreter::with_string_heap_cap(4 * 1024 * 1024).expect("fixture interpreter bootstrap");
    vm.gc_heap.set_gc_stress(0, false);
    let values = inputs(&mut vm, &source);
    let mut slots = [
        values[0],
        values[1],
        values[2],
        Value::undefined(),
        Value::undefined(),
    ];
    let mut frame = Frame::new(
        VmFrameHeader {
            function_id: 0,
            pc: 1,
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
    // SAFETY: the frame and its slot window outlive the operation and are
    // unpublished below before either drops.
    unsafe { vm.jit_push_native_frame(&mut frame) }.expect("publish fixture frame");
    // These deliberately differ from the lexical words. The fixed-value
    // semantic kernel must not consult the physical activation's bindings.
    let mut cell = std::ptr::from_mut(&mut frame) as u64;
    let record = SafepointRecord::window(31, NO_FRAME_STATE);
    let registry = CodeRegistryView {
        context: std::ptr::from_ref(&record) as u64,
        resolve_safepoint: resolve as *const () as u64,
        function_entries: 0,
        function_entry_count: 0,
        resolve_return_pc: 0,
    };
    let mut stack = crate::test_support::FrameChainFixture::new();
    let activation = crate::jit::VmRuntimeActivation::new(&mut vm, &mut stack, Some(&source));
    let mut thread = VmThread::empty();
    thread.frame_cell = std::ptr::from_mut(&mut cell) as u64;
    thread.runtime_context = std::ptr::from_ref(&activation) as u64;
    thread.code_registry = std::ptr::from_ref(&registry) as u64;
    let mut packet = RuntimeStubAllocContext::new(&mut thread, record.id);
    operation(&mut vm, &source, &mut frame, &mut packet, &mut slots);
    vm.jit_pop_native_frame();
}

#[test]
fn typed_closure_allocation_updates_every_explicit_lexical_input_at_strides_1_to_16() {
    for stride in 1..=16 {
        fixture(|vm, _, _, packet, slots| {
            let before_offsets = slots[..3]
                .iter()
                .map(|value| value.as_raw_gc().unwrap().0)
                .collect::<Vec<_>>();
            vm.gc_heap.set_gc_stress(stride, false);
            let before = vm.gc_heap.gc_stats().clone();
            for _ in 1..stride {
                vm.gc_heap.alloc(StressPrime { _word: 0 }).unwrap();
            }
            assert_eq!(
                vm.gc_heap.gc_stats().minor_gc_cycles,
                before.minor_gc_cycles,
                "prime cannot substitute for collection inside closure allocation"
            );
            let pair = vm.with_runtime_roots(|_| {
                make_closure_alloc(
                    packet,
                    31,
                    slots[0].to_bits(),
                    slots[1].to_bits(),
                    slots[2].to_bits(),
                )
            });
            assert_eq!(
                pair.validate(NativeResultDomain::Probe),
                Some(NativeResultStatus::Success)
            );
            let closure = pair
                .payload_value()
                .as_closure(&vm.gc_heap)
                .expect("actual arrow cell");
            assert_eq!(closure.cached_function_id, 2);
            assert_eq!(closure.context(&vm.gc_heap), slots[0]);
            assert_eq!(closure.bound_this(&vm.gc_heap), Some(slots[1]));
            assert_eq!(closure.bound_new_target(&vm.gc_heap), Some(slots[2]));
            assert_eq!(
                crate::context::read_slot(&vm.gc_heap, slots[0].as_context().unwrap(), 0),
                Some(Value::number_i32(404))
            );
            let after = vm.gc_heap.gc_stats().clone();
            assert!(
                after.minor_gc_cycles > before.minor_gc_cycles
                    && after.minor_root_slots_scanned > before.minor_root_slots_scanned
            );
            assert!(after.minor_slot_updates >= before.minor_slot_updates + 3);
            for (index, old) in before_offsets.iter().enumerate() {
                assert_ne!(
                    slots[index].as_raw_gc().unwrap().0,
                    *old,
                    "stride {stride} input {index} moved inside the typed stub"
                );
            }
        });
    }
}

#[test]
fn typed_function_registry_constructs_distinct_source_identities_and_refuses_invalid_packets() {
    for stub in [MAKE_FUNCTION_ALLOC, MAKE_CLOSURE_ALLOC] {
        assert!(!stub.is_valid_for_safepoint(NO_SAFEPOINT));
        assert!(stub.is_valid_for_safepoint(31));
        assert_eq!(
            alloc_value_stub_by_id(stub.descriptor.id)
                .unwrap()
                .entry_addr(),
            stub.entry_addr()
        );
    }
    fixture(|vm, _, frame, packet, slots| {
        frame.header.pc = 0;
        for slot in &mut slots[3..] {
            let pair = make_function_alloc(
                packet,
                31,
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits(),
            );
            assert_eq!(
                pair.validate(NativeResultDomain::Probe),
                Some(NativeResultStatus::Success)
            );
            *slot = pair.payload_value();
            let closure = slot.as_closure(&vm.gc_heap).unwrap();
            assert_eq!(closure.cached_function_id, 1);
            assert!(closure.context(&vm.gc_heap).is_undefined());
            assert_eq!(closure.bound_this(&vm.gc_heap), None);
            assert_eq!(closure.bound_new_target(&vm.gc_heap), None);
        }
        assert_ne!(
            slots[3], slots[4],
            "every evaluation creates a fresh identity"
        );
        let before = vm.gc_heap.gc_stats().alloc_bytes_total;
        for pair in [
            make_function_alloc(
                packet,
                31,
                slots[0].to_bits(),
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits(),
            ),
            make_closure_alloc(
                packet,
                31,
                slots[0].to_bits(),
                slots[1].to_bits(),
                slots[2].to_bits(),
            ),
            make_function_alloc(
                packet,
                NO_SAFEPOINT,
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits(),
            ),
            make_function_alloc(
                std::ptr::null_mut(),
                31,
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits(),
            ),
        ] {
            assert_eq!(
                pair.validate(NativeResultDomain::Probe),
                Some(NativeResultStatus::SideExit)
            );
        }
        frame.header.pc = 2;
        assert_eq!(
            make_function_alloc(
                packet,
                31,
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits(),
                Value::UNDEFINED.to_bits()
            )
            .validate(NativeResultDomain::Probe),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(
            vm.gc_heap.gc_stats().alloc_bytes_total,
            before,
            "all bad source/root/operand refusals precede allocation"
        );
    });
}

#[test]
fn typed_closure_oom_preserves_collector_updated_inputs_before_canonical_resume() {
    fixture(|vm, source, _, packet, slots| {
        let setup_heap = vm.gc_heap.stats();
        let context = slots[0].as_context().expect("initial scope context");
        let receiver = slots[1].as_object().expect("initial lexical receiver");
        let target = slots[2].as_object().expect("initial lexical target");
        // SAFETY: these three actual input cells are live; no allocation or
        // collection occurs before their authoritative header sizes are read.
        let input_bytes = unsafe {
            u64::from((*context.as_header_ptr()).size_bytes())
                + u64::from((*receiver.as_header_ptr()).size_bytes())
                + u64::from((*target.as_header_ptr()).size_bytes())
        };
        // Discard setup cells and settle all reclaimable bootstrap garbage
        // before creating the three fresh young inputs of the measured call.
        slots.fill(Value::undefined());
        vm.gc_heap.set_gc_stress(0, false);
        vm.with_runtime_roots(|vm| vm.gc_heap.collect_full(&mut |_| {}))
            .expect("preflight full collection removes reclaimable setup cells");
        let settled_heap = vm.gc_heap.stats();
        // Normal cap admission must reconcile accounting after the old-space
        // sweep. Its rooted retry precedes every fresh young input, and leaves
        // exactly their measured cell footprint plus one byte of headroom.
        let remaining =
            vm.gc_heap.max_heap_bytes() - settled_heap.allocated_bytes as u64 - input_bytes - 1;
        vm.with_runtime_roots(|vm| vm.gc_heap.reserve_bytes_with_roots(remaining, &mut |_| {}))
            .expect("rooted pressure admission reconciles the swept heap cap");
        let admitted_heap = vm.gc_heap.stats();
        assert_eq!(
            admitted_heap.tracked_bytes,
            admitted_heap.allocated_bytes as u64 + admitted_heap.reserved_bytes,
            "pressure is based on post-sweep occupied bytes: {admitted_heap:?}"
        );
        let setup_cycles = vm.gc_heap.gc_cycle_counts();
        slots[..3].copy_from_slice(&inputs(vm, source));
        let built_heap = vm.gc_heap.stats();
        assert_eq!(
            built_heap.allocated_bytes - admitted_heap.allocated_bytes,
            input_bytes as usize,
            "fresh source/lexical inputs use their exact admitted cell footprint"
        );
        assert_eq!(
            vm.gc_heap.gc_cycle_counts(),
            setup_cycles,
            "no setup collection may age or relocate the fresh inputs"
        );
        let original: Vec<_> = slots[..3]
            .iter()
            .map(|value| value.as_raw_gc().unwrap().0)
            .collect();
        vm.gc_heap.set_gc_stress(1, false);
        // Retire/refund any LAB before the only measured allocating call.
        // The reservation and every freshly built input must survive its GC.
        let fresh_heap = vm.gc_heap.stats();
        let before_heap = vm.gc_heap.stats();
        let before = vm.gc_heap.gc_stats().clone();
        let pair = vm.with_runtime_roots(|_| {
            make_closure_alloc(
                packet,
                31,
                slots[0].to_bits(),
                slots[1].to_bits(),
                slots[2].to_bits(),
            )
        });
        let after_heap = vm.gc_heap.stats();
        let after = vm.gc_heap.gc_stats().clone();
        let cap = vm.gc_heap.max_heap_bytes();
        vm.gc_heap.release_bytes(remaining);
        let closure_bytes =
            u64::from(crate::jit::JIT_CLOSURE_CELL_BYTES) + 2 * std::mem::size_of::<Value>() as u64;
        let status = pair.validate(NativeResultDomain::Probe);
        let evidence = format!(
            "status={status:?} cap={cap} reserve={remaining} input_bytes={input_bytes} closure_bytes={closure_bytes} \
             setup={setup_heap:?} settled={settled_heap:?} admitted={admitted_heap:?} fresh={fresh_heap:?} \
             before_heap={before_heap:?} after_heap={after_heap:?} \
             before_gc={before:?} after_gc={after:?}"
        );
        assert!(
            after_heap.tracked_bytes.saturating_add(closure_bytes) > cap,
            "post-collection pressure must refuse the exact arrow before checking OOM: {evidence}"
        );
        assert_eq!(
            status,
            Some(NativeResultStatus::OutOfMemory),
            "exact canonical closure cap outcome: {evidence}"
        );
        assert!(after.minor_gc_cycles > before.minor_gc_cycles);
        assert!(after.minor_root_slots_scanned > before.minor_root_slots_scanned);
        assert!(after.minor_slot_updates >= before.minor_slot_updates + 3);
        assert!(slots[1].is_object() && slots[2].is_object());
        assert_ne!(
            slots[1], slots[2],
            "lexical receiver and target remain distinct"
        );
        for (index, old) in original.iter().enumerate() {
            assert_ne!(
                slots[index].as_raw_gc().unwrap().0,
                *old,
                "OOM must leave caller home {index} rewritten before resume"
            );
        }
        assert_eq!(
            crate::context::read_slot(&vm.gc_heap, slots[0].as_context().unwrap(), 0),
            Some(Value::number_i32(404))
        );
    });
}
