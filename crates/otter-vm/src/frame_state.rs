//! Call-frame and pending-dispatch state for the VM interpreter.
//!
//! This module owns the data carried between dispatch-loop ticks: register
//! windows and resumable dispatch state. Async/generator ownership, active try
//! handlers, and protocol ladders live in the lazily attached cold record.
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
//! - Parked states own copied register snapshots and no arena pointers.
//! - GC-bearing frame and parked-state fields are visited by their tracers.
//! - Upvalue-spine construction traces both inherited and newly allocated cells
//!   until the completed spine is attached to a published frame.
//!
//! # Frame execution layout
//!
//! The optimizing tier bakes constant displacements against a frame's register
//! window and references the frame header by field. VM and JIT consume the same
//! current layout; there is no compatibility/versioned frame representation.
//!
//! - **Register window.** A frame's registers are a contiguous run of [`Value`]
//!   slots. Register `r` lives at `window_base + r * size_of::<Value>()`; the
//!   stride is 8 bytes ([`REGISTER_SLOT_BYTES`]). The base and arena offset
//!   always come from the frame's attached [`RegisterWindow`].
//! - **Calling convention.** Argument `i` (declaration order) is delivered in
//!   window register `i` for `i < arity`; the caller writes the arguments into
//!   the callee window starting at register 0 before transferring control. The
//!   prologue binds each into its local storage. Locals and scratch temporaries
//!   occupy registers above the arguments.
//! - **SELF / `this` are frame fields, not window registers.** They live in
//!   [`Frame::self_value`] / [`Frame::this_value`] and are materialized into a
//!   register on demand by the load opcodes, so a callee never reserves a
//!   window slot for them. `new.target` remains optional cold call state.
//! - **Header.** [`Frame::function_id`] + [`Frame::pc`] identify the resume
//!   point; [`Frame::return_register`] names the caller register that receives
//!   the completion value (`None` for `<main>`); [`Frame::upvalues`] is the
//!   captured-cell spine indexed by the upvalue opcodes.
//!
//! # See also
//! - [crate::frame_ops]
//! - [crate::executable]

use smallvec::SmallVec;

use otter_bytecode::Function;
use otter_gc::raw::{RawGc, SlotVisitor};

use crate::{
    CodeBlock, JsPromiseHandle, RegisterWindow, UpvalueCell, Value, VmError, VmFrameHeader,
    abstract_ops, cold_frame::ColdFrameIdx,
};

pub(crate) type UpvalueSpine = Box<[UpvalueCell]>;

/// Byte stride between adjacent registers in a frame window. Register `r` sits
/// at `window_base + r * REGISTER_SLOT_BYTES`. Frozen: the optimizing tier
/// bakes this stride into every windowed register access and the deopt record
/// reconstructs interpreter registers at this stride.
pub(crate) const REGISTER_SLOT_BYTES: usize = std::mem::size_of::<Value>();
const _: () = assert!(REGISTER_SLOT_BYTES == 8);

// One current 72-byte materialized activation. Async/generator ownership and
// all other uncommon protocol state live in `cold_frame::ColdFramePool` and are
// reached lazily through `frame.cold`.
const _: [(); 72] = [(); std::mem::size_of::<Frame>()];

/// Owned register values of a suspended frame. This type deliberately cannot
/// expose a [`RegisterWindow`]: parked state is independent of the active arena.
#[derive(Debug)]
pub struct OwnedRegisterSnapshot(SmallVec<[Value; 8]>);

impl OwnedRegisterSnapshot {
    #[must_use]
    pub(crate) fn copy_from(window: RegisterWindow) -> Self {
        Self(SmallVec::from_slice(&window))
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn trace_slots(&self, visitor: &mut SlotVisitor<'_>) {
        for value in &self.0 {
            value.trace_value_slots(visitor);
        }
    }
}

/// Materialized compatibility/cold-sidecar frame.
///
/// Existing HoltStack dispatch paths still own this compact record. New
/// tier-neutral execution uses [`crate::ActiveFrameMut`] over the canonical
/// [`crate::NativeFrame`] and its register window, so interpreter/baseline/
/// optimizer switches do not require constructing this Rust-owned adapter.
#[repr(C, align(8))]
#[derive(Debug)]
pub struct Frame {
    /// Common interpreter/baseline machine-visible frame prefix.
    pub header: VmFrameHeader,
    /// Register window for this frame.
    pub registers: RegisterWindow,
    /// Captured upvalues for this call. Empty for non-closure
    /// frames. Indexed by `Op::LoadUpvalue` / `Op::StoreUpvalue`
    /// operands.
    pub upvalues: UpvalueSpine,
    /// Exact function object executing this frame. Named-function SELF and
    /// `arguments.callee` read this hot field so materialized and native frames
    /// share one representation-neutral binding. Bare functions use the
    /// canonical interned function value; closure calls store that exact
    /// closure instance.
    pub self_value: Value,
    /// `this` value visible inside the body. `<main>` and free
    /// `Op::Call` invocations both bind `Value::Undefined`
    /// (foundation strict default). Method calls set the receiver,
    /// `Op::CallWithThis` and `Op::CallMethodValue` thread a caller-
    /// provided value, and arrow closures override with their
    /// lexically-captured `this` regardless of the call site.
    pub this_value: Value,
    /// When `Some(reg)`, returning from this frame writes the
    /// completion value into the **caller's** register `reg` and
    /// resumes at the caller's next pc. `<main>` carries `None`
    /// and propagates the value out as the script's completion.
    pub return_register: Option<u16>,
    /// Handle into the per-interpreter
    /// [`crate::cold_frame::ColdFramePool`] when this frame has
    /// acquired a cold side record (try handlers, async parking,
    /// pending ToPrimitive/bind/iterator ladders, …). `None` until
    /// the first opcode that needs cold state writes through
    /// [`crate::Interpreter::frame_ensure_cold`].
    pub cold: Option<ColdFrameIdx>,
}

/// GC-traceable off-stack frame ownership used by generators and async await.
/// It contains no pointer into [`crate::register_stack::RegisterStack`].
#[derive(Debug)]
pub struct ParkedFrameState {
    pub header: VmFrameHeader,
    registers: OwnedRegisterSnapshot,
    pub upvalues: UpvalueSpine,
    pub self_value: Value,
    pub this_value: Value,
    pub return_register: Option<u16>,
}

const _: [(); 0] = [(); std::mem::offset_of!(Frame, header)];
const _: [(); 16] = [(); std::mem::offset_of!(Frame, registers)];

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
    /// Result of `Get(target, "name")` once available.
    pub target_name: Option<Value>,
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

/// One active try-handler descriptor — the runtime counterpart to
/// the compiler's `TRY_BEGIN … TRY_END` block. Each
/// [`Op::EnterTry`] dispatch pushes one of these onto the
/// owning frame; throw unwinding pops back to the innermost match.
#[derive(Debug, Clone, Copy)]
pub struct TryHandler {
    /// Catch clause entry pc, or `None` for `try { … } finally { … }`
    /// without a catch.
    pub catch_pc: Option<u32>,
    /// Finally clause entry pc, or `None` when there is no
    /// finally. The unwinder routes the in-flight exception
    /// through finally even when a catch is present, so the
    /// compiler emits the catch body first and chains its
    /// completion through finally.
    pub finally_pc: Option<u32>,
    /// Register that the catch clause expects the thrown value in.
    /// Ignored when `catch_pc` is `None`.
    pub exc_register: u16,
}

impl Frame {
    /// Advance the canonical instruction-index program counter by one.
    /// Surfaces [`VmError::InvalidOperand`] on overflow.
    pub(crate) fn advance_pc(&mut self) -> Result<(), VmError> {
        crate::ActiveFrameMut::materialized(self).advance_pc()
    }

    /// Shared empty upvalue slice for plain functions without captured
    /// parent cells.
    pub(crate) fn empty_upvalues() -> UpvalueSpine {
        Vec::<UpvalueCell>::new().into_boxed_slice()
    }

    /// Allocate a frame for `function`. Registers are pre-filled
    /// with `Value::Undefined`. Used for test-side construction
    /// of trivial functions.
    ///
    /// **Precondition (since task 76):** `function.own_upvalue_count
    /// == 0`. Functions with own upvalues route through
    /// [`Self::for_function_with_heap`] (production path) or
    /// [`Self::build_upvalues`] + [`Self::with_return_upvalues_and_this`].
    #[must_use]
    pub fn for_function(function: &Function, window: RegisterWindow) -> Self {
        debug_assert_eq!(
            function.own_upvalue_count, 0,
            "Frame::for_function requires zero own upvalues — use for_function_with_heap or build_upvalues + with_return_upvalues_and_this"
        );
        Self::with_return(function, None, window)
    }

    /// Allocate a frame for `function`, allocating
    /// `function.own_upvalue_count` cells on the GC heap.
    /// The production entry path uses this for the `<main>`
    /// frame so any top-level `let n = 0; () => n` style upvalue
    /// has a backing cell from the moment dispatch starts.
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    pub fn for_function_with_heap(
        function: &Function,
        heap: &mut otter_gc::GcHeap,
        window: RegisterWindow,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let upvalues = Self::build_upvalues(heap, function, Self::empty_upvalues())?;
        Ok(Self::with_return_upvalues_and_this(
            function,
            None,
            upvalues,
            Value::undefined(),
            window,
        ))
    }

    /// Allocate a frame whose return value should land in the
    /// caller's register `return_register`. Same precondition as
    /// [`Self::for_function`] — zero own upvalues.
    #[must_use]
    pub fn with_return(
        function: &Function,
        return_register: Option<u16>,
        window: RegisterWindow,
    ) -> Self {
        Self::with_return_upvalues_and_this(
            function,
            return_register,
            Self::empty_upvalues(),
            Value::undefined(),
            window,
        )
    }

    /// Build the captured-upvalue spine for `function`, allocating
    /// `function.own_upvalue_count` fresh
    /// [`UpvalueCellBody`] cells on the GC heap and prepending them
    /// to `parent_upvalues` (per the §15.2.5 capture layout).
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    pub fn build_upvalues(
        heap: &mut otter_gc::GcHeap,
        function: &Function,
        parent_upvalues: UpvalueSpine,
    ) -> Result<UpvalueSpine, otter_gc::OutOfMemory> {
        let mut empty = |_: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
        Self::build_upvalues_for_count(
            heap,
            function.own_upvalue_count,
            parent_upvalues,
            &mut empty,
        )
    }

    pub(crate) fn build_upvalues_for_exec(
        heap: &mut otter_gc::GcHeap,
        function: &CodeBlock,
        parent_upvalues: UpvalueSpine,
    ) -> Result<UpvalueSpine, otter_gc::OutOfMemory> {
        let mut empty = |_: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
        Self::build_upvalues_for_count(
            heap,
            function.own_upvalue_count,
            parent_upvalues,
            &mut empty,
        )
    }

    /// [`Self::build_upvalues_for_exec`] with caller-owned roots exposed to
    /// any collection the cell allocations trigger. Frame builders hold the
    /// callee / receiver / `new.target` in plain Rust locals while the frame
    /// is not yet on a traced stack; those locals must ride through here.
    pub(crate) fn build_upvalues_for_exec_with_roots(
        heap: &mut otter_gc::GcHeap,
        function: &CodeBlock,
        parent_upvalues: UpvalueSpine,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<UpvalueSpine, otter_gc::OutOfMemory> {
        Self::build_upvalues_for_count(
            heap,
            function.own_upvalue_count,
            parent_upvalues,
            external_visit,
        )
    }

    fn build_upvalues_for_count(
        heap: &mut otter_gc::GcHeap,
        own_upvalue_count: u16,
        parent_upvalues: UpvalueSpine,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<UpvalueSpine, otter_gc::OutOfMemory> {
        let own = own_upvalue_count as usize;
        if own == 0 {
            return Ok(parent_upvalues);
        }
        let mut cells: Vec<UpvalueCell> = Vec::with_capacity(own + parent_upvalues.len());
        for _ in 0..own {
            // Neither collection is attached to a traced frame yet. A later
            // allocation can move an inherited or just-created cell, so expose
            // both live prefixes together with the caller's dynamic values.
            let mut build_roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                external_visit(visitor);
                for cell in &cells {
                    visitor(cell as *const UpvalueCell as *mut RawGc);
                }
                for cell in parent_upvalues.iter() {
                    visitor(cell as *const UpvalueCell as *mut RawGc);
                }
            };
            cells.push(crate::alloc_upvalue_with_roots(
                heap,
                Value::undefined(),
                &mut build_roots,
            )?);
        }
        cells.extend(parent_upvalues.iter().copied());
        Ok(cells.into_boxed_slice())
    }

    /// Full constructor used by call sites that need to bind a
    /// non-default `this`. The caller is responsible for
    /// pre-building `upvalues` via [`Self::build_upvalues`] (or
    /// passing [`Self::empty_upvalues`] when the function has none).
    /// See [`Op::MakeClosure`](otter_bytecode::Op::MakeClosure)
    /// for the layout.
    #[must_use]
    pub fn with_return_upvalues_and_this(
        function: &Function,
        return_register: Option<u16>,
        upvalues: UpvalueSpine,
        this_value: Value,
        window: RegisterWindow,
    ) -> Self {
        let total = function
            .param_count
            .saturating_add(function.locals)
            .saturating_add(function.scratch) as usize;
        debug_assert_eq!(window.len(), total);
        debug_assert!(
            upvalues.len() >= function.own_upvalue_count as usize,
            "frame upvalues must include the function's own cells"
        );
        Self {
            header: VmFrameHeader::interpreter(function.id, total as u16),
            registers: window,
            return_register,
            upvalues,
            self_value: Value::function(function.id),
            this_value,
            cold: None,
        }
    }

    #[must_use]
    pub(crate) fn with_exec_return_upvalues_and_this(
        function: &CodeBlock,
        return_register: Option<u16>,
        upvalues: UpvalueSpine,
        this_value: Value,
        window: RegisterWindow,
    ) -> Self {
        debug_assert_eq!(
            window.len(),
            function.register_count as usize,
            "register window must match the function's register_count"
        );
        debug_assert!(
            upvalues.len() >= function.own_upvalue_count as usize,
            "frame upvalues must include the function's own cells"
        );
        Self {
            header: VmFrameHeader::interpreter(function.id, function.register_count),
            registers: window,
            return_register,
            upvalues,
            self_value: Value::function(function.id),
            this_value,
            cold: None,
        }
    }

    /// Trace hot upvalues, SELF, and receiver state. Active register windows are
    /// traced once by RegisterStack; async/generator ownership is traced through
    /// the attached cold record.
    pub(crate) fn trace_frame_slots(&self, visitor: &mut SlotVisitor<'_>) {
        // Active register windows are traced once through RegisterStack's
        // precisely published prefix. The frame walker owns scalar/header state.
        for slot in self.upvalues.iter() {
            let p = slot as *const UpvalueCell as *mut RawGc;
            visitor(p);
        }
        self.self_value.trace_value_slots(visitor);
        self.this_value.trace_value_slots(visitor);
        // Cold-record GC slots (pending_to_primitive / pending_bind_function /
        // pending_iterator_next) are traced separately by the caller through
        // [`crate::cold_frame::ColdFrame::trace_cold_slots`] when
        // `self.cold` is `Some`.
    }
}

impl ParkedFrameState {
    /// Copy an active frame's values into owned suspension state. The caller
    /// must release `frame.registers` immediately after this returns.
    #[must_use]
    pub(crate) fn copy_from_active(frame: Frame) -> (Self, RegisterWindow) {
        let window = frame.registers;
        let registers = OwnedRegisterSnapshot::copy_from(window);
        (
            Self {
                header: frame.header,
                registers,
                upvalues: frame.upvalues,
                self_value: frame.self_value,
                this_value: frame.this_value,
                return_register: frame.return_register,
            },
            window,
        )
    }

    /// Move a parked snapshot into a newly reserved active window.
    #[must_use]
    pub(crate) fn into_active(self, mut window: RegisterWindow) -> Frame {
        debug_assert_eq!(window.len(), self.registers.len());
        window.copy_from_slice(&self.registers.0);
        let Self {
            header,
            registers: _,
            upvalues,
            self_value,
            this_value,
            return_register,
        } = self;
        Frame {
            header,
            registers: window,
            upvalues,
            self_value,
            this_value,
            return_register,
            cold: None,
        }
    }

    #[must_use]
    pub(crate) fn register_count(&self) -> usize {
        self.registers.len()
    }

    pub(crate) fn trace_slots(&self, visitor: &mut SlotVisitor<'_>) {
        self.registers.trace_slots(visitor);
        for slot in self.upvalues.iter() {
            let p = slot as *const UpvalueCell as *mut RawGc;
            visitor(p);
        }
        self.self_value.trace_value_slots(visitor);
        self.this_value.trace_value_slots(visitor);
    }

    #[cfg(test)]
    pub(crate) fn debug_register(&self, index: usize) -> Option<Value> {
        self.registers.0.get(index).copied()
    }
}
