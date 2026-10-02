//! Optimizing-tier JavaScript calls.
//!
//! # Contents
//! - [`emit`] — one `CallTarget::Direct` site: the actual span, the call ABI
//!   registers, the call into the proven target's current generation or the
//!   generic entry, and completion routing.
//!
//! # Invariants
//! - Roots are saved and the call site is stamped before this runs; every
//!   boxed input is read from its canonical safepoint home, because a staging
//!   or resolving entry may collect and move it.
//! - Method candidates only select the callee value; the callee's
//!   activation, receiver binding, construct completion and deoptimization
//!   belong to the callee.
//! - Nothing between the last staging entry and the call allocates.
//! - The completion is consumed once: `Success` stores the result, `Throw`
//!   reaches `threw` with the exception in `x0`, `Fatal` reaches `fatal` with
//!   the error parked in the context. Roots are reloaded on every path.
//!
//! # See also
//! - [`crate::arm64::js_call`] — the shared call ABI emitters.
//! - [`super::forward_call`] — argument-forwarding staging.

use super::*;
use crate::arm64::js_call::{
    CallTarget as JsCallTarget, emit_call, emit_pop_arguments, emit_push_arguments,
    emit_staged_call,
};
use otter_vm::native_abi::{STUB_JIT_RESOLVE_METHOD, STUB_JIT_STAGE_SPREAD};

/// Labels and layout shared by one call site's emission.
pub(super) struct CallSite<'a> {
    pub(super) view: &'a JitCompileSnapshot,
    pub(super) transitions: &'a TransitionTable,
    pub(super) sequence: &'a InstructionSequence,
    pub(super) instruction: &'a crate::machine::MachineInstruction,
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
    pub(super) fn load_operand(
        &self,
        ops: &mut dynasmrt::aarch64::Assembler,
        index: usize,
        register: u8,
        sp_bias: u32,
    ) -> Result<(), Unsupported> {
        let operand = self
            .instruction
            .operands
            .get(index)
            .ok_or(Unsupported::OperandShape("scalar call operand"))?;
        emit_load_safepoint_root(ops, self.frame, self.site, operand.value, register, sp_bias)
    }

    /// The register holding operand `index`, loading it into `scratch` when
    /// it lives elsewhere. While `current`, nothing has collected since the
    /// roots were saved, so an operand allocated to a register this sequence
    /// never writes is read in place; otherwise it is read from its home.
    pub(super) fn operand_register(
        &self,
        ops: &mut dynasmrt::aarch64::Assembler,
        index: usize,
        scratch: u8,
        sp_bias: u32,
        current: bool,
    ) -> Result<u8, Unsupported> {
        if current
            && let Some(AllocatedLocation::Register(register)) = self.locations.get(index)
            && register.is_integer()
            && register.encoding() > 17
        {
            return Ok(register.encoding());
        }
        self.load_operand(ops, index, scratch, sp_bias)?;
        Ok(scratch)
    }

    /// Route a staging entry's `(x0, x1)` completion: fall through on
    /// `Success`, otherwise reload roots and reach `threw` or `fatal`.
    pub(super) fn emit_staging_status(
        &self,
        ops: &mut dynasmrt::aarch64::Assembler,
    ) -> Result<(), Unsupported> {
        let staged = ops.new_dynamic_label();
        dynasm!(ops
            ; .arch aarch64
            ; cbz x1, =>staged
            ; mov x17, x0
            ; mov x15, x1
        );
        emit_reload_safepoint_roots(ops, self.frame, self.site)?;
        dynasm!(ops
            ; .arch aarch64
            ; cmp x15, NativeResultStatus::Throw as u32
            ; b.ne =>self.fatal
            ; mov x0, x17
            ; b =>self.threw
            ; =>staged
        );
        Ok(())
    }

    /// Call a context-first runtime stub at `entry`.
    pub(super) fn emit_stub_call(
        &self,
        ops: &mut dynasmrt::aarch64::Assembler,
        relocations: &mut RelocationCapture,
        stub: RuntimeStubDescriptor,
    ) {
        dynasm!(ops ; .arch aarch64 ; mov x0, x19);
        emit_load_symbolic_u64(
            ops,
            relocations,
            16,
            self.transitions.entry(stub),
            RelocationTarget::runtime_stub(stub),
        );
        dynasm!(ops ; .arch aarch64 ; blr x16);
    }

    /// Release `bytes` of pushed actuals after a call returned `(x0, x1)`,
    /// reload roots and route the completion.
    fn emit_complete(
        &self,
        ops: &mut dynasmrt::aarch64::Assembler,
        bytes: u32,
    ) -> Result<(), Unsupported> {
        let abrupt = ops.new_dynamic_label();
        emit_pop_arguments(ops, bytes);
        dynasm!(ops ; .arch aarch64 ; cbnz x1, =>abrupt);
        // Root reloads touch only root registers and x16, so the result
        // stays in x0.
        emit_reload_safepoint_roots(ops, self.frame, self.site)?;
        let result = *self
            .locations
            .get(self.result_index)
            .ok_or(Unsupported::OperandShape("scalar call result location"))?;
        emit_store_allocated_tagged(ops, self.frame, result, 0, 0)?;
        dynasm!(ops
            ; .arch aarch64
            ; b =>self.done
            ; =>abrupt
            ; mov x17, x0
            ; mov x15, x1
        );
        emit_reload_safepoint_roots(ops, self.frame, self.site)?;
        dynasm!(ops
            ; .arch aarch64
            ; cmp x15, NativeResultStatus::Throw as u32
            ; b.ne =>self.fatal
            ; mov x0, x17
            ; b =>self.threw
        );
        Ok(())
    }

    /// Call through the trampoline with the request a staging entry wrote
    /// completely, and complete.
    pub(super) fn emit_enter_and_complete(
        &self,
        ops: &mut dynasmrt::aarch64::Assembler,
        relocations: &mut RelocationCapture,
    ) -> Result<(), Unsupported> {
        crate::arm64::js_call::emit_enter_staged(ops, relocations, self.transitions, 19);
        self.emit_complete(ops, 0)
    }
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

/// Where the callee value of an invocation is.
#[derive(Clone, Copy)]
enum Callee {
    /// Operand zero.
    Operand,
    /// Selected by a method guard or resolution into this register.
    Register(u8),
}

/// Emit one direct call site.
pub(super) fn emit(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    call: &CallSite<'_>,
    kind: DirectCallKind,
    argument_mode: DirectCallArgumentMode,
    candidates: &[super::super::super::DirectCallCandidate],
) -> Result<(), Unsupported> {
    emit_load_u64(ops, 15, u64::from(call.call_pc));
    dynasm!(ops
        ; .arch aarch64
        ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
        ; str w15, [x16, NATIVE_FRAME_PC_OFFSET]
    );
    if kind == DirectCallKind::Forward {
        forward_call::emit_stage(ops, relocations, call)?;
        return call.emit_enter_and_complete(ops, relocations);
    }
    let result_index = call.result_index;
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
        if form.method || result_index != form.first_argument + 1 {
            return Err(Unsupported::OperandShape("scalar spread call operands"));
        }
        call.load_operand(ops, form.first_argument, 1, 0)?;
        call.emit_stub_call(ops, relocations, STUB_JIT_STAGE_SPREAD);
        call.emit_staging_status(ops)?;
        return emit_invoke(ops, relocations, call, form, None, Callee::Operand, false);
    }
    if form.method {
        // A candidate's guard proves the loaded method; a proven bytecode
        // method enters its current generation, any other receiver resolves
        // the method without calling it.
        for candidate in candidates {
            let guard = candidate
                .guard
                .as_ref()
                .ok_or(Unsupported::OperandShape("scalar method candidate guard"))?;
            let next = ops.new_dynamic_label();
            let receiver = call.operand_register(ops, 0, 9, 0, true)?;
            emit_method_guard_from_tagged_register(
                ops,
                relocations,
                call.view,
                guard,
                receiver,
                17,
                None,
                next,
            )?;
            // The guard proved the loaded method's function identity.
            let plan = candidate.callee.plan;
            dynasm!(ops ; .arch aarch64 ; mov x12, x17);
            emit_invoke(ops, relocations, call, form, Some(plan), Callee::Register(12), true)?;
            dynasm!(ops ; .arch aarch64 ; =>next);
        }
        let receiver = call
            .instruction
            .operands
            .first()
            .ok_or(Unsupported::OperandShape("scalar method receiver"))?
            .value;
        emit_value_span_arguments(ops, call.sequence, call.frame, call.site, [receiver].into_iter())?;
        call.emit_stub_call(ops, relocations, STUB_JIT_RESOLVE_METHOD);
        call.emit_staging_status(ops)?;
        dynasm!(ops ; .arch aarch64 ; mov x12, x0);
        return emit_invoke(ops, relocations, call, form, None, Callee::Register(12), false);
    }
    // One proven bytecode target enters its current generation; any other
    // callee enters the generic entry.
    if let [candidate] = candidates {
        let plan = candidate.callee.plan;
        let generic = ops.new_dynamic_label();
        let callee = call.operand_register(ops, 0, 9, 0, true)?;
        if callee != 9 {
            dynasm!(ops ; .arch aarch64 ; mov x9, X(callee));
        }
        crate::arm64::inline_guard::emit_cached_identity(
            ops,
            relocations,
            call.view,
            plan,
            call.call_pc,
            generic,
        );
        emit_invoke(ops, relocations, call, form, Some(plan), Callee::Operand, true)?;
        dynasm!(ops ; .arch aarch64 ; =>generic);
    }
    emit_invoke(ops, relocations, call, form, None, Callee::Operand, true)
}

/// Push the actuals, load callee/receiver/`new.target` and call either the
/// proven target's current generation or the generic entry; complete.
///
/// A proven call pads the span with `undefined` to the target's formal count.
/// A staged span is called through the trampoline with the request written
/// here. While `current`, no staging or resolving entry ran since the roots
/// were saved, so register operands are read in place.
fn emit_invoke(
    ops: &mut dynasmrt::aarch64::Assembler,
    relocations: &mut RelocationCapture,
    call: &CallSite<'_>,
    form: CallForm,
    known: Option<otter_vm::jit::JitDirectCallPlan>,
    callee: Callee,
    current: bool,
) -> Result<(), Unsupported> {
    let (bytes, count) = match form.argument_mode {
        DirectCallArgumentMode::Fixed => {
            let count = call
                .result_index
                .checked_sub(form.first_argument)
                .ok_or(Unsupported::OperandShape("scalar call argument count"))?;
            let pushed = known.map_or(count, |plan| count.max(usize::from(plan.param_count)));
            let bytes = emit_push_arguments(ops, pushed, |ops, index, register, bias| {
                if index < count {
                    call.operand_register(ops, form.first_argument + index, register, bias, current)
                } else {
                    emit_load_u64(ops, register, VALUE_UNDEFINED);
                    Ok(register)
                }
            })?;
            let count = u32::try_from(count)
                .map_err(|_| Unsupported::OperandShape("scalar call argument count"))?;
            (bytes, Some(count))
        }
        DirectCallArgumentMode::Spread => (0, None),
    };
    let callee = match callee {
        Callee::Register(register) => register,
        Callee::Operand => call.operand_register(ops, 0, 1, bytes, current)?,
    };
    let receiver = if form.method {
        Some(call.operand_register(ops, 0, 2, bytes, current)?)
    } else if form.explicit_receiver {
        Some(call.operand_register(ops, 1, 2, bytes, current)?)
    } else {
        None
    };
    let new_target = match form.new_target {
        None => None,
        Some(SuperTarget::Callee) => Some(callee),
        Some(SuperTarget::Frame) => {
            // `super(...)` forwards the running constructor's `new.target`.
            let ready = ops.new_dynamic_label();
            emit_load_u64(ops, 15, VALUE_UNDEFINED);
            dynasm!(ops
                ; .arch aarch64
                ; ldr x16, [x19, NATIVE_FRAME_OFFSET]
                ; ldr x3, [x16, crate::entry::NATIVE_FRAME_NEW_TARGET_OFFSET]
                ; cmp x3, x15
                ; b.ne =>ready
                ; mov x3, X(callee)
                ; =>ready
            );
            Some(3)
        }
    };
    match count {
        Some(count) => {
            // `[[Construct]]` enters a proven target directly only when it
            // has the internal method; classification throws otherwise.
            let target = known
                .filter(|plan| {
                    form.new_target.is_none()
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
                19,
                callee,
                receiver,
                new_target,
                count,
                target,
            );
        }
        None => emit_staged_call(
            ops,
            relocations,
            call.transitions,
            19,
            callee,
            receiver,
            new_target,
        ),
    }
    call.emit_complete(ops, bytes)
}
