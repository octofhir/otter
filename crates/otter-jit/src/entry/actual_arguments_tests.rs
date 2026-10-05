//! Executable actual-only JavaScript entry contract.
//!
//! # Contents
//! - Known and generic calls with zero, odd, adequate and extra actuals.
//! - Template window initialization and Graph lazy underarity entry.
//!
//! # Invariants
//! - The emitter reserves exactly the aligned actual span, never formal padding.
//! - Alignment slack contains poison, so a missing formal cannot come from it.
//! - Real compiled mappings, permanent cells and VM carriers outlive execution.
//! - Strict non-allocating bodies need no VM execution context or GC heap;
//!   moving roots and receiver conversion are tested in runtime.
//!
//! # See also
//! - `crate::arm64::frame` and `crate::x86_64::frame`.

use otter_bytecode::{Op, Operand};
use otter_vm::{
    JitCompileSnapshot, JitFunctionCode, Value,
    jit::JitTestInstruction,
    native_abi::{
        CallRequest, CodeEntryCell, CodeRegistryView, FUNCTION_CALL_NO_RECEIVER_CONVERSION,
        FunctionEntryCell, JitCtx, NativeResultPair, VmThread,
    },
};

#[cfg(target_arch = "aarch64")]
fn caller(
    transitions: &super::TransitionTable,
    cell: &FunctionEntryCell,
    actuals: &[Value],
    generic: bool,
) -> dynasmrt::ExecutableBuffer {
    use crate::arm64::js_call::{self, CallTarget};
    use crate::template::arm64::values::emit_load_u64;
    use dynasmrt::{DynasmApi, dynasm};
    let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
    let mut relocations = crate::artifact::relocation::RelocationCapture::default();
    dynasm!(ops ; .arch aarch64
        ; stp x29, x30, [sp, #-32]!
        ; stp x19, x20, [sp, #16]
        ; mov x20, x0
    );
    let bytes = js_call::emit_push_arguments(&mut ops, actuals.len(), |ops, i, scratch, _| {
        emit_load_u64(
            ops,
            scratch,
            NativeResultPair::success(actuals[i]).payload_bits(),
        );
        Ok(scratch)
    })
    .unwrap();
    if actuals.len() % 2 == 1 {
        emit_load_u64(
            &mut ops,
            9,
            NativeResultPair::success(Value::number_i32(999)).payload_bits(),
        );
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, (actuals.len() * 8) as u32]);
    }
    emit_load_u64(
        &mut ops,
        1,
        NativeResultPair::success(Value::function(1)).payload_bits(),
    );
    js_call::emit_call(
        &mut ops,
        &mut relocations,
        transitions,
        20,
        1,
        None,
        None,
        Some(actuals.len() as u32),
        if generic {
            CallTarget::Generic
        } else {
            CallTarget::Known {
                entry_cell: std::ptr::from_ref(cell) as u64,
                function_id: 1,
            }
        },
    );
    js_call::emit_pop_arguments(&mut ops, bytes);
    dynasm!(ops ; .arch aarch64
        ; ldp x19, x20, [sp, #16]
        ; ldp x29, x30, [sp], #32
        ; ret
    );
    ops.finalize().unwrap()
}

#[cfg(target_arch = "x86_64")]
fn caller(
    transitions: &super::TransitionTable,
    cell: &FunctionEntryCell,
    actuals: &[Value],
    generic: bool,
) -> dynasmrt::ExecutableBuffer {
    use crate::x86_64::js_call::{self, CallTarget};
    use dynasmrt::{DynasmApi, dynasm};
    let mut ops = dynasmrt::x64::Assembler::new().unwrap();
    let mut relocations = crate::artifact::relocation::RelocationCapture::default();
    // This private fixture uses the System V entry explicitly, including Windows.
    dynasm!(ops ; .arch x64 ; push r15 ; mov r15, rdi);
    let bytes = js_call::emit_push_arguments(&mut ops, actuals.len(), 11, |ops, i, scratch, _| {
        dynasm!(ops ; .arch x64 ; mov Rq(scratch), QWORD NativeResultPair::success(actuals[i]).payload_bits() as i64);
        Ok(())
    }).unwrap();
    if actuals.len() % 2 == 1 {
        dynasm!(ops ; .arch x64
            ; mov r11, QWORD NativeResultPair::success(Value::number_i32(999)).payload_bits() as i64
            ; mov [rsp + (actuals.len() * 8) as i32], r11
        );
    }
    dynasm!(ops ; .arch x64 ; mov rsi, QWORD NativeResultPair::success(Value::function(1)).payload_bits() as i64);
    js_call::emit_call(
        &mut ops,
        &mut relocations,
        transitions,
        15,
        false,
        false,
        actuals.len() as u32,
        if generic {
            CallTarget::Generic
        } else {
            CallTarget::Known {
                entry_cell: std::ptr::from_ref(cell) as u64,
                function_id: 1,
            }
        },
    );
    js_call::emit_pop_arguments(&mut ops, bytes);
    dynasm!(ops ; .arch x64 ; pop r15 ; ret);
    ops.finalize().unwrap()
}

#[test]
fn actual_only_known_and_generic_entries_initialize_missing_formals() {
    let transitions = super::TransitionTable::resolve();
    for returned in [0, 1, 2] {
        let mut snapshot = JitCompileSnapshot::without_feedback(
            1,
            3,
            3,
            vec![JitTestInstruction::new(
                Op::ReturnValue,
                0,
                0,
                vec![Operand::Register(returned)],
            )],
        );
        // The feedback-free fixture defaults to an observed sloppy receiver.
        // This body needs no receiver conversion or runtime execution context.
        std::sync::Arc::get_mut(&mut snapshot.code_block)
            .expect("the test snapshot uniquely owns its CodeBlock")
            .is_strict = true;
        let call_flags = snapshot.code_block.call_flags();
        assert_ne!(call_flags & FUNCTION_CALL_NO_RECEIVER_CONVERSION, 0);
        let template = crate::template::compile(&snapshot, 1, &transitions).unwrap();
        let codes: Vec<(Box<dyn JitFunctionCode>, u32)> = vec![(Box::new(template), 0)];
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        let codes = {
            let mut codes = codes;
            codes.push((
                Box::new(crate::optimizing::compile_optimized(&snapshot, 2, None).unwrap()),
                otter_vm::native_abi::CODE_ENTRY_HAS_SAFEPOINTS
                    | otter_vm::native_abi::CODE_ENTRY_OPTIMIZING_TIER,
            ));
            codes
        };
        for (code, flags) in &codes {
            let generation = CodeEntryCell::new(
                code.call_entry_addr().unwrap(),
                code.metadata().id,
                1,
                3,
                *flags,
                None,
            );
            let function = FunctionEntryCell::new(1, 3, 3, call_flags, 0);
            function.publish(std::ptr::from_ref(&generation) as u64);
            let directory = [0, std::ptr::from_ref(function.as_ref()) as u64];
            let registry = CodeRegistryView {
                context: 0,
                resolve_safepoint: 0,
                function_entries: directory.as_ptr() as u64,
                function_entry_count: 2,
                resolve_return_pc: 0,
            };
            for argc in 0..=5 {
                let actuals: Vec<_> = (0..argc)
                    .map(|i| Value::number_i32(100 + i as i32))
                    .collect();
                for generic in [false, true] {
                    let buffer = caller(&transitions, &function, &actuals, generic);
                    let interrupt = 0_u8;
                    let mut fuel = u64::MAX;
                    let mut thread = VmThread::empty();
                    thread.code_registry = std::ptr::from_ref(&registry) as u64;
                    thread.interrupt_cell = std::ptr::from_ref(&interrupt) as u64;
                    thread.backedge_fuel_cell = std::ptr::from_mut(&mut fuel) as u64;
                    let mut error = None;
                    let mut ctx = JitCtx {
                        thread: &mut thread,
                        native_frame: std::ptr::null_mut(),
                        error: &mut error,
                        generated_depth_limit: 8,
                        global_this_offset: std::ptr::null(),
                        native_stack_limit: 0,
                        generated_feedback_clean: 1,
                        completion_destination: u32::MAX,
                        completion_generation: 0,
                        alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
                        runtime_stats: std::ptr::null_mut(),
                        pending_call: CallRequest::EMPTY,
                        completion: NativeResultPair::success(Value::UNDEFINED),
                    };
                    thread.frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64;
                    #[cfg(target_arch = "aarch64")]
                    let entry: unsafe extern "C" fn(
                        *mut JitCtx,
                    )
                        -> NativeResultPair =
                        unsafe { std::mem::transmute(buffer.ptr(dynasmrt::AssemblyOffset(0))) };
                    #[cfg(target_arch = "x86_64")]
                    let entry: unsafe extern "sysv64" fn(
                        *mut JitCtx,
                    )
                        -> NativeResultPair =
                        unsafe { std::mem::transmute(buffer.ptr(dynasmrt::AssemblyOffset(0))) };
                    // SAFETY: the context, cells, executable mappings and actuals are live;
                    // bodies return an initialized formal without allocating or reentering.
                    let result = unsafe { entry(&mut ctx) };
                    let expected = actuals
                        .get(returned as usize)
                        .copied()
                        .unwrap_or(Value::UNDEFINED);
                    assert_eq!(
                        result,
                        NativeResultPair::success(expected),
                        "tier={:?} argc={argc} formal={returned} generic={generic}",
                        code.native_frame_kind()
                    );
                    assert!(ctx.native_frame.is_null());
                    assert!(error.is_none());
                    assert_eq!(
                        ctx.pending_call.entry, 0,
                        "generic underarity enters its compiled cell directly"
                    );
                }
            }
        }
    }
}
