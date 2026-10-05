//! Call-frame and pending-dispatch state for the VM interpreter.
//!
//! This module owns the data carried between dispatch-loop ticks: register
//! windows and resumable dispatch state. Async/generator ownership and
//! protocol ladders live in the lazily attached cold record.
//!
//! # Contents
//! - Active register windows and owned parked-frame snapshots.
//! - Pending state records for stack-modifying protocol drivers.
//! - Frame GC slot tracing.
//!
//! # Invariants
//! - Frame construction sizes registers from verified CodeBlock metadata.
//! - Frame and pending-record PCs are dense CodeBlock instruction indexes.
//! - Every active frame owns one attached [`RegisterWindow`].
//! - Parked states own copied register snapshots and no active frame pointers.
//! - GC-bearing frame and parked-state fields are visited by their tracers.
//! - Every frame is constructed with its exact SELF: the closure (or bare
//!   function value) being executed. There is no default; context-reading
//!   bytecode (`LoadClosureContext`, `LoadSelf`) reads it directly.
//! - Frames hold no binding storage: captured and eval-visible bindings live
//!   in contexts. A nullable own DerivedThis identity root proves terminal
//!   ownership without copying or caching a binding value.
//!
//! # Frame execution layout
//!
//! The optimizing tier bakes constant displacements against a frame's register
//! window and references the frame header by field. VM and JIT consume the same
//! current layout; there is no compatibility/versioned frame representation.
//!
//! - **Register window.** A frame's registers are a contiguous run of [`Value`]
//!   slots. Register `r` lives at `window_base + r * size_of::<Value>()`; the
//!   stride is 8 bytes ([`REGISTER_SLOT_BYTES`]). The base and capacity
//!   come from the frame's attached [`RegisterWindow`].
//! - **Calling convention.** Argument `i` (declaration order) is delivered in
//!   window register `i` for `i < arity`; the caller writes the arguments into
//!   the callee window starting at register 0 before transferring control. The
//!   prologue binds each into its local storage. Locals and scratch temporaries
//!   occupy registers above the arguments.
//! - **SELF / `this` are frame fields, not window registers.** They live in
//!   [`Frame::self_value`] / [`Frame::this_value`] and are materialized into a
//!   register on demand by the load opcodes, so a callee never reserves a
//!   window slot for them. `new.target` is stored in the same activation record.
//! - **Header.** [`Frame::function_id`] + [`Frame::pc`] identify the resume
//!   point; [`Frame::return_register`] names the caller register that receives
//!   the completion value (`None` for `<main>`).
//!
//! # See also
//! - [crate::frame_ops]
//! - [crate::executable]

use smallvec::SmallVec;

use otter_bytecode::Function;
use otter_gc::raw::SlotVisitor;

use crate::{
    JsPromiseHandle, RegisterWindow, Value, VmError, abstract_ops,
    native_abi::{Frame, VmFrameHeader},
};

/// Byte stride between adjacent registers in a frame window. Register `r` sits
/// at `window_base + r * REGISTER_SLOT_BYTES`. Frozen: the optimizing tier
/// bakes this stride into every windowed register access and the deopt record
/// reconstructs interpreter registers at this stride.
pub(crate) const REGISTER_SLOT_BYTES: usize = std::mem::size_of::<Value>();
const _: () = assert!(REGISTER_SLOT_BYTES == 8);

/// Owned register values of a suspended frame. This type deliberately cannot
/// expose a [`RegisterWindow`]: parked state is independent of the native stack.
#[derive(Debug)]
pub struct OwnedRegisterSnapshot(SmallVec<[Value; 8]>);

impl OwnedRegisterSnapshot {
    pub(crate) fn trace_slots(&self, visitor: &mut SlotVisitor<'_>) {
        for value in &self.0 {
            value.trace_value_slots(visitor);
        }
    }

    fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        for value in &self.0 {
            crate::code_liveness::visit_value(value, visitor);
        }
    }
}

/// GC-traceable off-stack frame ownership used by generators and async await.
/// It owns copied values independently of the published native stack.
#[derive(Debug)]
pub struct ParkedFrameState {
    pub header: VmFrameHeader,
    registers: OwnedRegisterSnapshot,
    arguments: SmallVec<[Value; 8]>,
    parameter_count: u16,
    pub self_value: Value,
    pub this_value: Value,
    pub new_target_value: Value,
    pub arguments_object: crate::JsObject,
    pub(crate) construct_layout: crate::constructor_layout::ConstructorLayout,
    pub(crate) derived_this_context: crate::context::ContextHandle,
    pub(crate) construct_receiver: Value,
    pub return_register: Option<u16>,
}

impl std::ops::Deref for Frame {
    type Target = VmFrameHeader;

    fn deref(&self) -> &Self::Target {
        &self.header
    }
}

impl std::ops::DerefMut for Frame {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.header
    }
}

/// In-flight state for [`Op::GetIterator`] when the source operand
/// is a user object. Carries the originating `pc` (so the resume
/// guard can verify) and the destination register that should
/// receive the [`Value::Iterator`] handle on completion.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-getiterator>
#[derive(Debug, Clone)]
pub struct PendingGetIterator {
    /// pc of the originating `Op::GetIterator`.
    pub pc: u32,
    /// Destination register the iterator handle must land in.
    pub dst: u16,
}

/// In-flight state for [`Op::IteratorNext`] over a user iterator.
/// The dispatcher calls `iter.next()` and parks this record with
/// the destination registers for `value` and `done` plus the
/// scratch register that received the call's result record.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-iteratornext>
#[derive(Debug, Clone)]
pub struct PendingIteratorNext {
    /// pc of the originating `Op::IteratorNext`.
    pub pc: u32,
    /// Destination register for the unpacked `value`.
    pub value_dst: u16,
    /// Destination register for the unpacked `done` flag.
    pub done_dst: u16,
    /// Scratch register that receives the `iter.next()` result
    /// record. The resume step reads `value` / `done` off this
    /// register and clears the slot.
    pub result_reg: u16,
    /// The iterator value itself. Cloned onto the parked record
    /// so the resume step can transition the inner state to
    /// [`IteratorState::Exhausted`] once `done` becomes truthy.
    pub iterator: Value,
}

/// In-flight state for an [`Op::ToPrimitive`] dispatch.
///
/// Carries the original object operand, the resolved hint, the
/// destination register the ladder writes its final result into,
/// and the next stage to run when the dispatcher resumes. Cloning
/// is cheap: every payload is either a small enum variant or a
/// `Value` clone.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-toprimitive>
/// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
#[derive(Debug, Clone)]
pub struct PendingToPrimitive {
    /// pc of the originating `Op::ToPrimitive` — so the resume
    /// hook can verify the dispatcher is back on the same
    /// instruction.
    pub pc: u32,
    /// Destination register for the final primitive value.
    pub dst: u16,
    /// Original (object) operand.
    pub obj: Value,
    /// Caller's preferred-type hint.
    pub hint: abstract_ops::ToPrimitiveHint,
    /// Next stage to attempt.
    pub stage: ToPrimitiveStage,
}

/// In-flight state for [`Op::BindFunction`] while collecting the
/// target callable's observable metadata.
#[derive(Debug, Clone)]
pub struct PendingBindFunction {
    /// pc of the originating `Op::BindFunction`.
    pub pc: u32,
    /// Destination register for the bound function and temporary
    /// getter return values.
    pub dst: u16,
    /// Callable being bound.
    pub target: Value,
    /// Bound `this` value captured from the call.
    pub bound_this: Value,
    /// Bound leading arguments captured from the call.
    pub bound_args: SmallVec<[Value; 4]>,
    /// Current metadata getter stage.
    pub stage: PendingBindStage,
    /// Result of the first metadata read (`Get(target, "length")`)
    /// once available.
    pub target_length: Option<Value>,
}

/// Metadata stage currently awaited by [`PendingBindFunction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingBindStage {
    /// Awaiting / about to read `target.name`.
    Name,
    /// Awaiting / about to read `target.length`.
    Length,
}

/// Stages of the §7.1.1 / §7.1.1.1 ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToPrimitiveStage {
    /// About to look up `[Symbol.toPrimitive]` and (if callable)
    /// invoke it.
    SymbolToPrim,
    /// Resuming from `[Symbol.toPrimitive]`; non-primitive results
    /// throw instead of falling through to the ordinary chain.
    SymbolResult,
    /// First slot of the OrdinaryToPrimitive chain — `valueOf` for
    /// `Default` / `Number` hints, `toString` for `String` hint.
    OrdinaryFirst,
    /// Second slot — `toString` after `valueOf`, or `valueOf` after
    /// `toString`.
    OrdinarySecond,
    /// Both ordinary slots have run and returned non-primitive
    /// values. The next dispatch tick raises `TypeMismatch` per
    /// §7.1.1.1 step 6.
    Exhausted,
}

/// Per-frame bookkeeping for an async-function call. Constructed
/// by the entry path in [`Interpreter::invoke`] when the callee's
/// [`otter_bytecode::Function::is_async`] flag is true; consumed by
/// [`Interpreter::pop_frame`] (fulfilment) and the throw-unwinder
/// (rejection).
#[derive(Debug, Clone)]
pub struct AsyncFrameState {
    /// The promise the call-site received synchronously. Settles
    /// when the async body returns (fulfil) or throws an
    /// unhandled error (reject).
    pub result_promise: JsPromiseHandle,
}

impl Frame {
    /// Advance the canonical instruction-index program counter by one.
    /// Surfaces [`VmError::InvalidOperand`] on overflow.
    pub(crate) fn advance_pc(&mut self) -> Result<(), VmError> {
        crate::ActiveFrameMut::from_frame(self).advance_pc()
    }

    /// Advance the canonical PC by one with a direct field write, skipping the
    /// interpreter dispatch wrapper and the overflow branch of [`Self::advance_pc`].
    /// A PC past the end of the instruction stream is caught by the next
    /// instruction fetch (`instr_at_index` → `MissingReturn`), so the hot path
    /// needs no explicit overflow check.
    #[inline]
    pub(crate) fn advance_pc_fast(&mut self) {
        self.pc = self.pc.wrapping_add(1);
    }

    /// Frame for a bytecode [`Function`] record with an explicit SELF and
    /// receiver. Hosts and tests that hold only the compiler DTO use this;
    /// dispatch builds frames from verified [`CodeBlock`]s through
    /// [`Self::for_code_block`].
    #[must_use]
    pub fn for_function(
        function: &Function,
        return_register: Option<u16>,
        self_value: Value,
        this_value: Value,
        window: RegisterWindow,
    ) -> Self {
        let total = function
            .param_count
            .saturating_add(function.locals)
            .saturating_add(function.scratch) as usize;
        debug_assert_eq!(window.len(), total);
        let mut frame = Self::new(
            VmFrameHeader::interpreter(function.id, total as u16),
            window.as_mut_ptr() as u64,
            self_value,
            this_value,
        );
        frame.registers = window;
        frame.set_return_register(return_register);
        frame
    }

    /// Trace the common non-register roots of this published activation.
    /// Register windows and cold protocol records are visited by their owners.
    pub(crate) fn trace_frame_slots(&self, visitor: &mut SlotVisitor<'_>) {
        // SAFETY: activation publication keeps this record initialized and
        // live. The collector owns all in-place relocation writes.
        unsafe { Self::trace_fields(std::ptr::from_ref(self).cast_mut(), visitor) };
    }
}

impl ParkedFrameState {
    /// Copy a published activation into owned suspension data.
    #[must_use]
    pub(crate) fn copy_from_active(frame: &Frame) -> Self {
        let view = crate::ActiveFrameRef::from_frame(frame);
        let count = view.incoming_argument_count();
        let arguments = (0..count)
            .map(|index| {
                view.incoming_argument(index)
                    .expect("published argument bounds")
            })
            .collect();
        let mut parked = Self::from_inputs(
            frame.header,
            SmallVec::from_slice(&frame.registers),
            arguments,
            0,
            frame.self_value,
            frame.this_value,
            frame.new_target(),
            frame.arguments_object,
            frame.return_register(),
        );
        parked.construct_layout = frame.construct_layout;
        parked.derived_this_context = frame.derived_this_context;
        parked.construct_receiver = frame.construct_receiver;
        parked
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_inputs(
        header: VmFrameHeader,
        registers: SmallVec<[Value; 8]>,
        arguments: SmallVec<[Value; 8]>,
        parameter_count: u16,
        self_value: Value,
        this_value: Value,
        new_target_value: Value,
        arguments_object: crate::JsObject,
        return_register: Option<u16>,
    ) -> Self {
        Self {
            header,
            registers: OwnedRegisterSnapshot(registers),
            arguments,
            parameter_count,
            self_value,
            this_value,
            new_target_value,
            arguments_object,
            construct_layout: crate::constructor_layout::ConstructorLayout::null(),
            derived_this_context: crate::context::ContextHandle::null(),
            construct_receiver: Value::UNDEFINED,
            return_register,
        }
    }

    /// Resume through the same native-stack packet as an ordinary call.
    pub(crate) fn into_prepared(self) -> crate::PreparedCall {
        crate::PreparedCall {
            header: self.header,
            self_value: self.self_value,
            this_value: self.this_value,
            new_target_value: self.new_target_value,
            arguments_object: self.arguments_object,
            construct_layout: self.construct_layout,
            derived_this_context: self.derived_this_context,
            construct_receiver: self.construct_receiver,
            return_register: self.return_register,
            cold: None,
            parameter_count: self.parameter_count,
            arguments: self.arguments,
            initial_registers: self.registers.0,
            child: None,
            resume: crate::prepared_call::ResumeInput::Normal,
        }
    }

    pub(crate) fn trace_slots(&self, visitor: &mut SlotVisitor<'_>) {
        self.registers.trace_slots(visitor);
        for value in &self.arguments {
            value.trace_value_slots(visitor);
        }
        self.self_value.trace_value_slots(visitor);
        self.this_value.trace_value_slots(visitor);
        self.new_target_value.trace_value_slots(visitor);
        self.construct_receiver.trace_value_slots(visitor);
        if !self.derived_this_context.is_null() {
            visitor(
                std::ptr::addr_of!(self.derived_this_context)
                    .cast_mut()
                    .cast(),
            );
        }
        if !self.construct_layout.is_null() {
            visitor(std::ptr::addr_of!(self.construct_layout).cast_mut().cast());
        }
        if !self.arguments_object.is_null() {
            visitor(
                std::ptr::addr_of!(self.arguments_object)
                    .cast_mut()
                    .cast::<otter_gc::raw::RawGc>(),
            );
        }
    }

    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        visitor(self.header.function_id);
        self.registers.visit_function_ids(visitor);
        for value in &self.arguments {
            crate::code_liveness::visit_value(value, visitor);
        }
        crate::code_liveness::visit_value(&self.self_value, visitor);
        crate::code_liveness::visit_value(&self.this_value, visitor);
        crate::code_liveness::visit_value(&self.new_target_value, visitor);
        crate::code_liveness::visit_value(&self.construct_receiver, visitor);
        // The active source and real SELF above are semantic code roots.
        // construct_layout.base_function_id is deliberately not visited.
    }

    #[cfg(test)]
    pub(crate) fn debug_register(&self, index: usize) -> Option<Value> {
        self.registers.0.get(index).copied()
    }
}
