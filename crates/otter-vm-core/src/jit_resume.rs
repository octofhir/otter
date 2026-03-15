//! Bridge helpers for resuming the interpreter from JIT deopt/OSR state.
//!
//! This module centralizes the runtime handoff back from JIT into interpreter
//! structures so deopt resume, OSR continuation, and generator yield-bailout
//! materialization do not stay duplicated across interpreter entry points.

use std::sync::Arc;

use otter_vm_bytecode::{Function, Instruction, Module};

use crate::context::VmContext;
use crate::generator::{GeneratorFrame, TryEntry};
use crate::jit_runtime::JitResumeState;
use crate::value::{UpvalueCell, Value};

pub(crate) struct GeneratorYieldResume {
    pub(crate) yielded_value: Value,
    pub(crate) frame: GeneratorFrame,
}

fn build_window_from_resume_state(
    ctx: &VmContext,
    local_count: usize,
    reg_count: usize,
    state: &JitResumeState,
) -> Vec<Value> {
    let mut window = Vec::with_capacity(local_count + reg_count);
    for i in 0..local_count {
        window.push(
            ctx.get_local(i as u16)
                .unwrap_or_else(|_| Value::undefined()),
        );
    }
    for i in 0..reg_count {
        window.push(*ctx.get_register(i as u16));
    }
    for slot in &state.locals {
        if let Some(entry) = window.get_mut(slot.index as usize) {
            *entry = slot.value;
        }
    }
    for slot in &state.registers {
        if let Some(entry) = window.get_mut(local_count + slot.index as usize) {
            *entry = slot.value;
        }
    }
    window
}

pub(crate) fn resume_in_place(ctx: &mut VmContext, state: &JitResumeState) {
    ctx.restore_deopt_state(state.bailout_pc, &state.locals, &state.registers);
    // Note: explicit feedback widening is NOT needed here. The interpreter
    // will naturally record wider type observations when it executes the bailed
    // instruction. The reason-aware bailout threshold (TypeGuardFailure → 3)
    // ensures fast recompilation with the wider feedback.
}

pub(crate) fn try_materialize_generator_yield(
    ctx: &VmContext,
    state: &JitResumeState,
    func: &Function,
    func_index: u32,
    module: Arc<Module>,
    upvalues: Vec<UpvalueCell>,
    try_stack: Vec<TryEntry>,
    this_value: Value,
    is_construct: bool,
    argc: u16,
) -> Option<GeneratorYieldResume> {
    let instructions = func.instructions.read();
    let Some(Instruction::Yield { dst, src }) = instructions.get(state.bailout_pc as usize) else {
        return None;
    };

    let yielded_value = state
        .registers
        .iter()
        .find(|slot| slot.index == src.0)
        .map(|slot| slot.value)
        .unwrap_or_else(|| *ctx.get_register(src.0));
    let local_count = func.local_count as usize;
    let reg_count = func.register_count as usize;
    let window = build_window_from_resume_state(ctx, local_count, reg_count, state);

    Some(GeneratorYieldResume {
        yielded_value,
        frame: GeneratorFrame::with_yield_dst(
            state.bailout_pc as usize + 1,
            func_index,
            module,
            local_count,
            window,
            upvalues,
            try_stack,
            this_value,
            is_construct,
            0,
            argc,
            dst.0,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit_runtime::DeoptValueSlot;
    use crate::runtime::VmRuntime;
    use otter_vm_bytecode::Register;

    fn build_test_module(instructions: Vec<Instruction>) -> Arc<Module> {
        let mut builder = Module::builder("jit_resume.js");
        let func = instructions.into_iter().fold(
            Function::builder()
                .name("resume_target")
                .local_count(2)
                .register_count(2),
            |builder, instruction| builder.instruction(instruction),
        );
        builder.add_function(func.build());
        Arc::new(builder.build())
    }

    #[test]
    fn build_window_from_resume_state_overrides_sparse_slots() {
        let runtime = VmRuntime::new();
        let mut ctx = runtime.create_context();
        let module = build_test_module(vec![Instruction::Return { src: Register(0) }]);
        ctx.register_module(&module);
        ctx.push_frame(0, module.module_id, 2, None, false, false, 0)
            .unwrap();
        ctx.set_local(0, Value::int32(1)).unwrap();
        ctx.set_local(1, Value::int32(2)).unwrap();
        ctx.set_register(0, Value::int32(10));
        ctx.set_register(1, Value::int32(11));

        let state = JitResumeState {
            bailout_pc: 5,
            bailout_reason: otter_vm_jit::BailoutReason::Unknown,
            locals: vec![DeoptValueSlot {
                index: 1,
                value: Value::int32(99),
            }],
            registers: vec![DeoptValueSlot {
                index: 0,
                value: Value::int32(77),
            }],
        };

        let window = build_window_from_resume_state(&ctx, 2, 2, &state);
        assert_eq!(window[0], Value::int32(1));
        assert_eq!(window[1], Value::int32(99));
        assert_eq!(window[2], Value::int32(77));
        assert_eq!(window[3], Value::int32(11));
    }

    #[test]
    fn try_materialize_generator_yield_builds_sparse_resume_frame() {
        let runtime = VmRuntime::new();
        let mut ctx = runtime.create_context();

        let mut builder = Module::builder("jit_generator_resume.js");
        let func = Function::builder()
            .name("generator_resume")
            .local_count(1)
            .register_count(3)
            .instruction(Instruction::Yield {
                dst: Register(2),
                src: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        builder.add_function(func);
        let module = Arc::new(builder.build());
        ctx.register_module(&module);
        ctx.push_frame(0, module.module_id, 1, None, false, false, 0)
            .unwrap();
        ctx.set_local(0, Value::int32(5)).unwrap();
        ctx.set_register(0, Value::int32(10));
        ctx.set_register(1, Value::int32(20));
        ctx.set_register(2, Value::int32(30));

        let state = JitResumeState {
            bailout_pc: 0,
            bailout_reason: otter_vm_jit::BailoutReason::Unknown,
            locals: vec![DeoptValueSlot {
                index: 0,
                value: Value::int32(55),
            }],
            registers: vec![DeoptValueSlot {
                index: 1,
                value: Value::int32(200),
            }],
        };

        let resume = try_materialize_generator_yield(
            &ctx,
            &state,
            &module.functions[0],
            0,
            Arc::clone(&module),
            Vec::new(),
            Vec::new(),
            Value::undefined(),
            false,
            0,
        )
        .expect("yield bailout should materialize a generator frame");

        assert_eq!(resume.yielded_value, Value::int32(200));
        assert_eq!(resume.frame.pc, 1);
        assert_eq!(resume.frame.local_count, 1);
        assert_eq!(resume.frame.window.len(), 4);
        assert_eq!(resume.frame.window[0], Value::int32(55));
        assert_eq!(resume.frame.window[1], Value::int32(10));
        assert_eq!(resume.frame.window[2], Value::int32(200));
        assert_eq!(resume.frame.window[3], Value::int32(30));
        assert_eq!(resume.frame.yield_dst, Some(2));
    }
}
