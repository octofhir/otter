//! Executable NativeFunction kind guards and exact current selector sizes.
//!
//! # Contents
//! - Every header byte plus immediate, numeric and function-id collisions.
//! - Source preservation for each real callee register used by both tiers.
//! - Exact emitted caller bytes and zero-instruction VM selector coordinates.
//!
//! # Invariants
//! - Local headers prove only classification, never NativeCtx execution or GC.
//! - Every non-cell input must reject before the header read.
//! - Runtime proofs independently exercise the real Host kernel and moving roots.
//! - Byte counts describe this binary and never stand for elapsed performance.

use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::value::tag;

#[cfg(target_arch = "aarch64")]
fn classifier(value: u8) -> crate::CompiledCode {
    let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let hit = ops.new_dynamic_label();
    let changed = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; mov x15, x0 ; mov x17, 85 ; mov X(value), x0);
    crate::arm64::js_call::emit_native_kind_guard(&mut ops, value, miss);
    dynasm!(ops ; .arch aarch64 ; b =>hit ; =>miss
        ; cmp x17, 85 ; b.ne =>changed
        ; cmp X(value), x15 ; b.ne =>changed ; mov w0, 0 ; ret
        ; =>hit ; cmp x17, 85 ; b.ne =>changed
        ; cmp X(value), x15 ; b.ne =>changed ; mov w0, 1 ; ret
        ; =>changed ; mov w0, 2 ; ret);
    crate::CompiledCode::new(ops.finalize().unwrap(), entry)
}

#[cfg(target_arch = "x86_64")]
fn classifier(value: u8) -> crate::CompiledCode {
    let mut ops = dynasmrt::x64::Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let changed = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; mov Rq(value), rdi);
    crate::x86_64::js_call::emit_native_kind_guard(&mut ops, value, miss);
    dynasm!(ops ; .arch x64
        ; cmp Rq(value), rdi ; jne =>changed ; mov eax, 1 ; ret
        ; =>miss ; cmp Rq(value), rdi ; jne =>changed ; xor eax, eax ; ret
        ; =>changed ; mov eax, 2 ; ret);
    crate::CompiledCode::new(ops.finalize().unwrap(), entry)
}

#[test]
fn native_kind_rejects_non_cells_and_preserves_callee_on_every_branch() {
    #[cfg(target_arch = "aarch64")]
    let registers = [0_u8, 1, 9, 12];
    #[cfg(target_arch = "x86_64")]
    let registers = [0_u8, 2, 6, 7, 8, 9];
    for register in registers {
        let code = classifier(register);
        // SAFETY: no calls, allocation or nonvolatile register writes occur;
        // only masked, non-null cells below can reach their retained local byte.
        #[cfg(target_arch = "aarch64")]
        let run: extern "C" fn(u64) -> u32 = unsafe { std::mem::transmute(code.entry_ptr()) };
        #[cfg(target_arch = "x86_64")]
        let run: extern "sysv64" fn(u64) -> u32 = unsafe { std::mem::transmute(code.entry_ptr()) };
        for bits in [
            0,
            tag::VALUE_NULL,
            tag::VALUE_UNDEFINED,
            tag::VALUE_TRUE,
            tag::VALUE_FALSE,
            tag::VALUE_HOLE,
            tag::box_int32(0),
            tag::box_int32(-1),
            tag::box_double((-0.0_f64).to_bits()),
            tag::box_double(1.25_f64.to_bits()),
            tag::box_double(f64::NAN.to_bits()),
            tag::box_double(f64::INFINITY.to_bits()),
        ] {
            assert_eq!(run(bits), 0, "noncell {bits:016x} in GP{register}");
        }
        for id in [0, 1, 0xffff, u32::MAX] {
            assert_eq!(run(tag::box_function_id(id)), 0);
            for bit in 49..64 {
                assert_eq!(run((1_u64 << bit) | tag::box_function_id(id)), 0);
            }
        }
        for actual in 0..=255_u8 {
            let header = [u64::from(actual), 0];
            let cell = header.as_ptr() as u64;
            assert_eq!(cell & tag::NOT_CELL_MASK, 0);
            assert_eq!(
                run(cell),
                u32::from(actual == otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG)
            );
            // High immediate/number bits must still reject, even when the low
            // address points at a header with the desired native byte.
            assert_eq!(run(cell | tag::NUMBER_TAG), 0);
            assert_eq!(run(cell | tag::OTHER_TAG), 0);
        }
    }
}

#[test]
fn reports_exact_caller_and_shared_entry_spans_without_timing_claims() {
    use crate::artifact::relocation::RelocationCapture;
    use crate::call_linkage::CallTarget;
    let table = super::TransitionTable::resolve();
    #[derive(Debug)]
    struct CallerBytes {
        guard: usize,
        hit_call: usize,
        miss_call: usize,
        control: usize,
        total: usize,
    }
    let mut costs = Vec::new();
    for selected in [false, true] {
        #[cfg(target_arch = "aarch64")]
        let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
        #[cfg(target_arch = "x86_64")]
        let mut ops = dynasmrt::x64::Assembler::new().unwrap();
        let mut relocations = RelocationCapture::new(false);
        let miss = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        let start = ops.offset().0;
        if selected {
            #[cfg(target_arch = "aarch64")]
            crate::arm64::js_call::emit_native_kind_guard(&mut ops, 1, miss);
            #[cfg(target_arch = "x86_64")]
            crate::x86_64::js_call::emit_native_kind_guard(&mut ops, 6, miss);
        }
        let guard = ops.offset().0 - start;
        let mut emit = |ops: &mut _, target| {
            #[cfg(target_arch = "aarch64")]
            let actual_return = crate::arm64::js_call::emit_call(
                ops,
                &mut relocations,
                &table,
                0,
                1,
                Some(2),
                Some(3),
                Some(3),
                target,
            );
            #[cfg(target_arch = "x86_64")]
            let actual_return = crate::x86_64::js_call::emit_call(
                ops,
                &mut relocations,
                &table,
                15,
                true,
                true,
                3,
                target,
            );
            actual_return
        };
        let target = if selected {
            CallTarget::Native
        } else {
            CallTarget::Generic
        };
        let hit_return = emit(&mut ops, target);
        assert_eq!(hit_return.0, ops.offset().0);
        let hit_call = hit_return.0 - start - guard;
        let mut miss_call = 0;
        if selected {
            #[cfg(target_arch = "aarch64")]
            dynasm!(ops ; .arch aarch64 ; b =>done ; =>miss);
            #[cfg(target_arch = "x86_64")]
            dynasm!(ops ; .arch x64 ; jmp =>done ; =>miss);
            let miss_start = ops.offset().0;
            let miss_return = emit(&mut ops, CallTarget::Generic);
            assert_eq!(miss_return.0, ops.offset().0);
            miss_call = miss_return.0 - miss_start;
        } else {
            dynasm!(ops ; =>miss);
        }
        dynasm!(ops ; =>done);
        let total = ops.offset().0 - start;
        costs.push(CallerBytes {
            guard,
            hit_call,
            miss_call,
            control: total - guard - hit_call - miss_call,
            total,
        });
        ops.finalize().unwrap();
    }
    let vm = otter_vm::native_abi::native_entry_code_sizes();
    assert_eq!(costs[0].guard, 0);
    assert_eq!(costs[0].miss_call, 0);
    assert_eq!(costs[0].control, 0);
    assert!(costs[1].guard > 0 && costs[1].control > 0);
    assert_eq!(
        costs[0].hit_call, costs[1].hit_call,
        "same private operands and stub call width"
    );
    assert_eq!(costs[1].miss_call, costs[0].hit_call);
    assert_eq!(costs[0].total, costs[0].hit_call);
    assert_eq!(
        costs[1].total,
        costs[1].guard + costs[1].hit_call + costs[1].control + costs[1].miss_call
    );
    assert!(vm.generic_selector_bytes > vm.native_selector_bytes);
    assert!(vm.request_publisher_bytes > vm.native_header_bytes && vm.native_header_bytes > 0);
    assert!(vm.classifier_bytes > 0 && vm.reservation_bytes > 0);
    assert!(vm.trampoline_bytes > vm.classifier_bytes + vm.reservation_bytes);
    println!(
        "native-kind caller={costs:?}; vm={vm:?}; sharedHostKernel=host_call_entry; rustKernelExtent=needsBinarySymbolEvidence; staticBytesOnly=true"
    );
}

#[test]
fn same_source_whole_compilation_exposes_both_hit_and_miss_code_cost() {
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, JitFunctionCode, JitNativeCall, jit::JitTestInstruction};
    fn view(selected: bool) -> JitCompileSnapshot {
        let mut view = JitCompileSnapshot::without_feedback(
            7,
            4,
            5,
            vec![
                JitTestInstruction::new(
                    Op::Call,
                    0,
                    0,
                    vec![
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::ConstIndex(3),
                        Operand::Register(1),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 24, vec![Operand::Register(4)]),
            ],
        );
        std::sync::Arc::get_mut(&mut view.code_block)
            .unwrap()
            .is_strict = true;
        view.instructions[0].call_attempted = true;
        if selected {
            view.native_calls.insert(0, JitNativeCall::Native);
        }
        view
    }
    let table = super::TransitionTable::resolve();
    let mut sizes = Vec::new();
    for selected in [false, true] {
        let view = view(selected);
        // Compile-only source selection: no address is called, no policy or
        // execution feedback is simulated. Runtime owns actual admission.
        let template = crate::template::compile(&view, 31, &table).unwrap();
        let graph = crate::graph::compile(&view, 32, &table, None, false).unwrap();
        let plans: Vec<_> = graph
            .built
            .graph
            .nodes
            .iter()
            .filter_map(|node| {
                if let crate::graph::ir::Kind::CallJs { plan, .. } = &node.kind {
                    Some(*plan)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            plans,
            [if selected {
                crate::call_linkage::CallPlan::Native
            } else {
                crate::call_linkage::CallPlan::Generic
            }]
        );
        sizes.push((
            template.metadata().code_size as usize,
            graph.emission.buffer.len(),
        ));
    }
    assert!(
        sizes[1].0 > sizes[0].0 && sizes[1].1 > sizes[0].1,
        "kind specialization retains extra guard, selected hit and complete Generic miss"
    );
    println!(
        "native-kind sameSource=(TemplateBytes,GraphBytes) generic={:?} selected={:?}; staticBytesOnly=true",
        sizes[0], sizes[1]
    );
}
