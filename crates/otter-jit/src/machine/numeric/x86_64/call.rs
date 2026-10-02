//! Optimizing-tier JavaScript calls on x86-64.
//!
//! # Contents
//! - [`emit`] — one `CallTarget::Direct` site: the actual span, the call ABI
//!   registers, the call into the proven target's current generation or the
//!   generic entry, and completion routing.
//!
//! # Invariants
//! - Roots are saved and the call site is stamped before this runs; every
//!   boxed input is read from its canonical safepoint home.
//! - Only caller-saved registers are used: values the allocator keeps in
//!   callee-saved registers across the call stay intact.
//! - Method candidates only select the callee value; the callee's
//!   activation, receiver binding, construct completion and deoptimization
//!   belong to the callee.
//! - The completion is consumed once: `Success` reloads the roots and
//!   stores the result from `rax`; an abrupt completion is parked in the red
//!   zone while roots reload, then `Throw` reaches `threw` with the exception
//!   in `rax` and `Fatal` reaches `fatal`.
//!
//! # See also
//! - [`crate::x86_64::js_call`] — the shared call ABI emitters.

use super::*;
use crate::machine::{
    DirectCallArgumentMode, DirectCallCandidate, DirectCallKind, MachineInstruction,
};
use crate::x86_64::js_call::{
    CallTarget as JsCallTarget, emit_call, emit_enter_staged, emit_pop_arguments,
    emit_push_arguments, emit_staged_call,
};
use otter_vm::native_abi::{
    STUB_JIT_FORWARD_SOURCE_READY, STUB_JIT_RESOLVE_METHOD, STUB_JIT_STAGE_FORWARD,
    STUB_JIT_STAGE_SPREAD,
};

/// Labels and layout shared by one call site's emission.
pub(super) struct CallSite<'a> {
    pub(super) view: &'a JitCompileSnapshot,
    pub(super) transitions: &'a TransitionTable,
    pub(super) sequence: &'a InstructionSequence,
    pub(super) instruction: &'a MachineInstruction,
    pub(super) frame: MachineFrameLayout,
    pub(super) site: &'a MachineSafepointSite,
    pub(super) locations: &'a [AllocatedLocation],
    /// Operand index of the call's result; every lower operand is an input.
    pub(super) result_index: usize,
    pub(super) call_pc: u32,
    pub(super) deopt: DynamicLabel,
    pub(super) threw: DynamicLabel,
    pub(super) fatal: DynamicLabel,
    pub(super) done: DynamicLabel,
}

impl CallSite<'_> {
    /// Load operand `index` from its canonical safepoint home.
    fn load_operand(
        &self,
        ops: &mut Assembler,
        index: usize,
        register: u8,
        bias: u32,
    ) -> Result<(), Unsupported> {
        let operand = self
            .instruction
            .operands
            .get(index)
            .ok_or(Unsupported::OperandShape("x86-64 call operand"))?;
        let root = self
            .site
            .roots
            .iter()
            .find(|root| root.value == operand.value)
            .ok_or(Unsupported::OperandShape("x86-64 call operand root"))?;
        let offset = root_offset(self.frame, root.save_slot)?
            .checked_add(bias)
            .and_then(|offset| i32::try_from(offset).ok())
            .ok_or(Unsupported::OperandShape("x86-64 call operand offset"))?;
        dynasm!(ops ; .arch x64 ; mov Rq(register), [rsp + offset]);
        Ok(())
    }

    fn emit_stub_call(
        &self,
        ops: &mut Assembler,
        relocations: &mut RelocationCapture,
        stub: RuntimeStubDescriptor,
    ) {
        dynasm!(ops ; .arch x64 ; mov rdi, r15);
        runtime(ops, relocations, self.transitions.entry(stub), stub);
        dynasm!(ops ; .arch x64 ; call r11);
    }

    /// Route a staging entry's `rax`/`rdx` completion: fall through on
    /// `Success` with the payload in `rax`, otherwise reload roots and reach
    /// `threw` or `fatal`.
    fn emit_staging_status(&self, ops: &mut Assembler) -> Result<(), Unsupported> {
        let staged = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch x64
            ; test rdx, rdx
            ; jz =>staged
            ; mov [rsp - 8], rax
            ; mov [rsp - 16], rdx
        );
        reload_roots(ops, self.frame, self.site)?;
        dynasm!(ops
            ; .arch x64
            ; mov rax, [rsp - 8]
            ; cmp QWORD [rsp - 16], NativeResultStatus::Throw as i32
            ; jne =>self.fatal
            ; jmp =>self.threw
            ; =>staged
        );
        Ok(())
    }

    /// Release `bytes` of pushed actuals after a call returned `rax`/`rdx`,
    /// reload roots and route the completion.
    fn emit_complete(&self, ops: &mut Assembler, bytes: u32) -> Result<(), Unsupported> {
        let abrupt = ops.new_dynamic_label();
        emit_pop_arguments(ops, bytes);
        dynasm!(ops ; .arch x64 ; test rdx, rdx ; jnz =>abrupt);
        // Root reloads use only root registers and `r11`, so the result stays
        // in `rax`.
        reload_roots(ops, self.frame, self.site)?;
        let result = *self
            .locations
            .get(self.result_index)
            .ok_or(Unsupported::OperandShape("x86-64 call result location"))?;
        store_integer(ops, self.frame, result, 0)?;
        dynasm!(ops
            ; .arch x64
            ; jmp =>self.done
            ; =>abrupt
            ; mov [rsp - 8], rax
            ; mov [rsp - 16], rdx
        );
        reload_roots(ops, self.frame, self.site)?;
        dynasm!(ops
            ; .arch x64
            ; mov rax, [rsp - 8]
            ; cmp QWORD [rsp - 16], NativeResultStatus::Throw as i32
            ; jne =>self.fatal
            ; jmp =>self.threw
        );
        Ok(())
    }

    /// Fill a `count`-word packet in the frame's raw scratch through
    /// `load(ops, index, register)` and leave its address in `rsi` and its
    /// length in `edx`.
    fn emit_value_span<Load>(
        &self,
        ops: &mut Assembler,
        count: usize,
        mut load: Load,
    ) -> Result<(), Unsupported>
    where
        Load: FnMut(&mut Assembler, usize, u8) -> Result<(), Unsupported>,
    {
        let count = u16::try_from(count)
            .map_err(|_| Unsupported::OperandShape("x86-64 value-span length"))?;
        if count == 0 {
            dynasm!(ops ; .arch x64 ; xor esi, esi ; xor edx, edx);
            return Ok(());
        }
        let packet = super::super::value_packet_frame(self.sequence)?;
        let end = packet
            .raw_start
            .checked_add(count)
            .ok_or(Unsupported::OperandShape("x86-64 value-span extent"))?;
        if count > packet.raw_words || end > self.frame.raw_slots() {
            return Err(Unsupported::OperandShape("x86-64 value-span capacity"));
        }
        for index in 0..count {
            load(ops, usize::from(index), 0)?;
            let offset = raw(self.frame, packet.raw_start + index)?;
            dynasm!(ops ; .arch x64 ; mov [rsp + offset], rax);
        }
        let offset = raw(self.frame, packet.raw_start)?;
        dynasm!(ops
            ; .arch x64
            ; lea rsi, [rsp + offset]
            ; mov edx, i32::from(count)
        );
        Ok(())
    }
}

fn raw(frame: MachineFrameLayout, slot: u16) -> Result<i32, Unsupported> {
    frame
        .raw_offset(slot)
        .ok()
        .and_then(|offset| i32::try_from(offset).ok())
        .ok_or(Unsupported::OperandShape("x86-64 raw scratch offset"))
}

/// Operand shape of one call form.
#[derive(Clone, Copy)]
struct CallForm {
    first_argument: usize,
    explicit_receiver: bool,
    method: bool,
    new_target: Option<SuperTarget>,
    argument_mode: DirectCallArgumentMode,
}

/// `new.target` of a construct form.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SuperTarget {
    /// `new C(...)`: the constructor itself.
    Callee,
    /// `super(...)`: the running constructor's `new.target`.
    Frame,
}

/// Emit one direct call site.
pub(super) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    call: &CallSite<'_>,
    kind: DirectCallKind,
    argument_mode: DirectCallArgumentMode,
    candidates: &[DirectCallCandidate],
) -> Result<(), Unsupported> {
    dynasm!(ops
        ; .arch x64
        ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov DWORD [r10 + NATIVE_FRAME_PC_OFFSET as i32], call.call_pc as i32
    );
    if kind == DirectCallKind::Forward {
        emit_forward_stage(ops, relocations, call)?;
        emit_enter_staged(ops, relocations, call.transitions, 15);
        return call.emit_complete(ops, 0);
    }
    let explicit_receiver = matches!(
        kind,
        DirectCallKind::CallWithThis | DirectCallKind::FunctionCall
    );
    let form = CallForm {
        first_argument: if explicit_receiver { 2 } else { 1 },
        explicit_receiver,
        method: kind == DirectCallKind::Method,
        new_target: match kind {
            DirectCallKind::Construct | DirectCallKind::DerivedConstruct => {
                Some(SuperTarget::Callee)
            }
            DirectCallKind::SuperConstruct | DirectCallKind::DerivedSuperConstruct => {
                Some(SuperTarget::Frame)
            }
            _ => None,
        },
        argument_mode,
    };
    if argument_mode == DirectCallArgumentMode::Spread {
        if form.method || call.result_index != form.first_argument + 1 {
            return Err(Unsupported::OperandShape("x86-64 spread call operands"));
        }
        call.load_operand(ops, form.first_argument, 6, 0)?;
        call.emit_stub_call(ops, relocations, STUB_JIT_STAGE_SPREAD);
        call.emit_staging_status(ops)?;
        return emit_invoke(ops, relocations, call, form, None, false);
    }
    if form.method {
        // A candidate's guard proves the loaded method; a proven bytecode
        // method enters its current generation, any other receiver resolves
        // the method without calling it.
        for candidate in candidates {
            let guard = candidate
                .guard
                .as_ref()
                .ok_or(Unsupported::OperandShape("x86-64 method candidate guard"))?;
            let next = ops.new_dynamic_label();
            call.load_operand(ops, 0, 9, 0)?;
            emit_inline_method_guard(ops, relocations, call.view, guard, next)?;
            emit_invoke(
                ops,
                relocations,
                call,
                form,
                Some(candidate.callee.plan),
                true,
            )?;
            dynasm!(ops ; .arch x64 ; =>next);
        }
        let receiver = call
            .instruction
            .operands
            .first()
            .ok_or(Unsupported::OperandShape("x86-64 method receiver"))?
            .value;
        call.emit_value_span(ops, 1, |ops, _, register| {
            load_saved_root(ops, call.frame, call.site, receiver, register)
        })?;
        call.emit_stub_call(ops, relocations, STUB_JIT_RESOLVE_METHOD);
        call.emit_staging_status(ops)?;
        dynasm!(ops ; .arch x64 ; mov r9, rax);
        return emit_invoke(ops, relocations, call, form, None, true);
    }
    // One proven bytecode target enters its current generation; any other
    // callee enters the generic entry.
    if let [candidate] = candidates {
        let plan = candidate.callee.plan;
        let generic = ops.new_dynamic_label();
        call.load_operand(ops, 0, 9, 0)?;
        crate::x86_64::js_call::emit_cached_identity(
            ops,
            relocations,
            plan,
            call.call_pc,
            generic,
            |ops, bail| emit_inline_identity(ops, call.view, plan.function_id, bail),
        );
        emit_invoke(ops, relocations, call, form, Some(plan), false)?;
        dynasm!(ops ; .arch x64 ; =>generic);
    }
    emit_invoke(ops, relocations, call, form, None, false)
}

/// Push the actuals, load callee/receiver/`new.target` into the call ABI
/// registers and call either the proven target's current generation or the
/// generic entry; complete.
///
/// `callee_in_r9` names a callee already selected by a method guard or
/// resolution; otherwise operand zero is the callee.
fn emit_invoke(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    call: &CallSite<'_>,
    form: CallForm,
    known: Option<otter_vm::jit::JitDirectCallPlan>,
    callee_in_r9: bool,
) -> Result<(), Unsupported> {
    let (bytes, count) = match form.argument_mode {
        DirectCallArgumentMode::Fixed => {
            let count = call
                .result_index
                .checked_sub(form.first_argument)
                .ok_or(Unsupported::OperandShape("x86-64 call argument count"))?;
            let pushed = known.map_or(count, |plan| count.max(usize::from(plan.param_count)));
            let bytes = emit_push_arguments(ops, pushed, 0, |ops, index, register, bias| {
                if index < count {
                    call.load_operand(ops, form.first_argument + index, register, bias)
                } else {
                    dynasm!(ops ; .arch x64 ; mov Rd(register), VALUE_UNDEFINED as i32);
                    Ok(())
                }
            })?;
            let count = u32::try_from(count)
                .map_err(|_| Unsupported::OperandShape("x86-64 call argument count"))?;
            (bytes, Some(count))
        }
        DirectCallArgumentMode::Spread => (0, None),
    };
    if callee_in_r9 {
        dynasm!(ops ; .arch x64 ; mov rsi, r9);
    } else {
        call.load_operand(ops, 0, 6, bytes)?;
    }
    let receiver = if form.method {
        call.load_operand(ops, 0, 2, bytes)?;
        true
    } else if form.explicit_receiver {
        call.load_operand(ops, 1, 2, bytes)?;
        true
    } else {
        false
    };
    match form.new_target {
        None => {}
        Some(SuperTarget::Callee) => dynasm!(ops ; .arch x64 ; mov rcx, rsi),
        Some(SuperTarget::Frame) => {
            // `super(...)` forwards the running constructor's `new.target`.
            let ready = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch x64
                ; mov r10, [r15 + NATIVE_FRAME_OFFSET as i32]
                ; mov rcx, [r10 + NATIVE_FRAME_NEW_TARGET_OFFSET as i32]
                ; cmp rcx, VALUE_UNDEFINED as i32
                ; jne =>ready
                ; mov rcx, rsi
                ; =>ready
            );
        }
    }
    let new_target = form.new_target.is_some();
    match count {
        Some(count) => {
            // `[[Construct]]` enters a proven target directly only when it
            // has the internal method; classification throws otherwise.
            let target = known
                .filter(|plan| {
                    !new_target
                        || plan.call_flags & otter_vm::native_abi::FUNCTION_CALL_CONSTRUCTIBLE != 0
                })
                .map_or(JsCallTarget::Generic, |plan| JsCallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                });
            emit_call(
                ops,
                relocations,
                call.transitions,
                15,
                receiver,
                new_target,
                count,
                target,
            );
        }
        None => emit_staged_call(ops, relocations, call.transitions, 15, receiver, new_target),
    }
    call.emit_complete(ops, bytes)
}

/// Admit the forwarding source, then stage the forwarded call's complete
/// request from `[method, callee, receiver, register bindings…, formals
/// context]`; a source the staging entry cannot complete exits before any
/// effect.
fn emit_forward_stage(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    call: &CallSite<'_>,
) -> Result<(), Unsupported> {
    call.load_operand(ops, 0, 6, 0)?;
    call.emit_stub_call(ops, relocations, STUB_JIT_FORWARD_SOURCE_READY);
    let admitted = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; test rax, rax ; jnz =>admitted);
    reload_roots(ops, call.frame, call.site)?;
    dynasm!(ops ; .arch x64 ; jmp =>call.deopt ; =>admitted);
    let words = call.result_index;
    let context_register = call.view.code_block.forwarded_formals_context();
    call.emit_value_span(ops, words, |ops, index, register| match context_register {
        Some(frame_register) if index + 1 == words => {
            dynasm!(ops
                ; .arch x64
                ; mov Rq(register), [r15 + NATIVE_FRAME_OFFSET as i32]
                ; mov Rq(register), [Rq(register) + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
                ; mov Rq(register), [Rq(register) + i32::from(frame_register) * 8]
            );
            Ok(())
        }
        _ => call.load_operand(ops, index, register, 0),
    })?;
    call.emit_stub_call(ops, relocations, STUB_JIT_STAGE_FORWARD);
    call.emit_staging_status(ops)
}
