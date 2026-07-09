//! Call-frame and pending-dispatch state for the VM interpreter.
//!
//! This module owns the data carried between dispatch-loop ticks: register
//! windows, active try handlers, async/generator side state, and resumable
//! protocol ladders such as ToPrimitive, bind metadata, and iterator stepping.
//!
//! # Contents
//! - Frame register windows and return metadata.
//! - Pending state records for stack-modifying protocol drivers.
//! - Frame GC slot tracing.
//!
//! # Invariants
//! - Frame construction sizes registers from bytecode/executable metadata.
//! - Pending records identify their originating pc before resume.
//! - GC-bearing frame fields are visited by trace_frame_slots.
//!
//! # Frame ABI (frozen)
//!
//! The optimizing tier bakes constant displacements against a frame's register
//! window and references the frame header by field, and the deopt frame-state
//! record is keyed by these slots, so the following layout is a stable contract
//! — change a literal only in lockstep with the codegen and the deopt record.
//!
//! - **Register window.** A frame's registers are a contiguous run of [`Value`]
//!   slots. Register `r` lives at `window_base + r * size_of::<Value>()`; the
//!   stride is 8 bytes ([`REGISTER_SLOT_BYTES`]). For a [`FrameRegisters::Window`]
//!   the base is `ptr` and `base_off` is its slot index in the flat register
//!   stack; an [`FrameRegisters::Owned`] window has the same per-register layout
//!   in its own buffer.
//! - **Calling convention.** Argument `i` (declaration order) is delivered in
//!   window register `i` for `i < arity`; the caller writes the arguments into
//!   the callee window starting at register 0 before transferring control. The
//!   prologue binds each into its local storage. Locals and scratch temporaries
//!   occupy registers above the arguments.
//! - **`this` / new.target are header fields, not window registers.** They live
//!   in [`Frame::this_value`] (and the cold record) and are materialized into a
//!   register on demand by the load opcodes, so a callee never reserves a window
//!   slot for them.
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
    ExecutableFunction, JsPromiseHandle, UpvalueCell, Value, VmError, abstract_ops,
    cold_frame::ColdFrameIdx,
};

pub(crate) type UpvalueSpine = Box<[UpvalueCell]>;

/// Byte stride between adjacent registers in a frame window. Register `r` sits
/// at `window_base + r * REGISTER_SLOT_BYTES`. Frozen: the optimizing tier
/// bakes this stride into every windowed register access and the deopt record
/// reconstructs interpreter registers at this stride.
pub(crate) const REGISTER_SLOT_BYTES: usize = std::mem::size_of::<Value>();
const _: () = assert!(REGISTER_SLOT_BYTES == 8);

// Hot frame fits in two 64 B cache lines. Cold protocol state (try
// handlers, async parking, pending ToPrimitive/bind/iterator ladders,
// rest/incoming args, …) lives in `cold_frame::ColdFramePool` and is
// reached lazily through `frame.cold`.
const _: () = assert!(
    std::mem::size_of::<Frame>() <= 144,
    "hot Frame must stay within ~2 cache lines; cold-state fields belong in ColdFrame",
);

/// A frame's register window: inline-owned (interpreter and parked frames) or a
/// slice into the interpreter's flat register stack (the JIT direct-call path,
/// which builds callee windows in machine code without a Rust bridge). Derefs to
/// `[Value]` so register access is uniform.
#[derive(Debug)]
pub enum FrameRegisters {
    /// Inline-owned window — every interpreter-created frame, and any frame that
    /// may be parked (generators / async), since a parked window must survive
    /// while other frames push/pop the flat stack.
    Owned(SmallVec<[Value; 8]>),
    /// A `[ptr, ptr+len)` window into the interpreter's reserved (never
    /// reallocating) flat register stack, built by the JIT direct-call path. A
    /// `Window` frame is never parked (the direct-call eligibility check rejects
    /// generators / async), so its window is only ever popped when the frame ends.
    Window {
        /// First slot of the window in the flat register stack.
        ptr: *mut Value,
        /// Window length (the callee's register count).
        len: u16,
        /// Slot index of `ptr` in the flat register stack. Popping the frame
        /// truncates the stack cursor back to here.
        base_off: u32,
    },
}

impl FrameRegisters {
    /// Slot index of a `Window` in the flat register stack, or `None` for an
    /// inline-owned buffer.
    #[inline]
    #[must_use]
    pub(crate) fn window_base(&self) -> Option<u32> {
        match self {
            FrameRegisters::Owned(_) => None,
            FrameRegisters::Window { base_off, .. } => Some(*base_off),
        }
    }
}

impl FrameRegisters {
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[Value] {
        match self {
            FrameRegisters::Owned(v) => v,
            // SAFETY: a `Window` points at `len` live slots in the reserved flat
            // register stack, valid for the frame's life (the stack never
            // reallocates; the window is popped only when the frame ends).
            FrameRegisters::Window { ptr, len, .. } => unsafe {
                std::slice::from_raw_parts(*ptr, *len as usize)
            },
        }
    }

    #[inline]
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [Value] {
        match self {
            FrameRegisters::Owned(v) => v,
            // SAFETY: see `as_slice`; `&mut self` gives exclusive access.
            FrameRegisters::Window { ptr, len, .. } => unsafe {
                std::slice::from_raw_parts_mut(*ptr, *len as usize)
            },
        }
    }

    /// Raw pointer to the first slot (the JIT frame register base).
    #[inline]
    #[must_use]
    pub fn as_mut_ptr(&mut self) -> *mut Value {
        match self {
            FrameRegisters::Owned(v) => v.as_mut_ptr(),
            FrameRegisters::Window { ptr, .. } => *ptr,
        }
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            FrameRegisters::Owned(v) => v.len(),
            FrameRegisters::Window { len, .. } => *len as usize,
        }
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::ops::Deref for FrameRegisters {
    type Target = [Value];
    #[inline]
    fn deref(&self) -> &[Value] {
        self.as_slice()
    }
}

impl std::ops::DerefMut for FrameRegisters {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Value] {
        self.as_mut_slice()
    }
}

impl Default for FrameRegisters {
    fn default() -> Self {
        FrameRegisters::Owned(SmallVec::new())
    }
}

impl Clone for FrameRegisters {
    /// Always clones to `Owned` (a `Window` aliases the flat stack and must not
    /// be duplicated). Cloning is a cold path (frame park / generator save) that
    /// needs an owned copy anyway.
    fn clone(&self) -> Self {
        FrameRegisters::Owned(SmallVec::from_slice(self.as_slice()))
    }
}

/// One call frame. Compact and cache-conscious per foundation
/// plan §M7. Slice 13 promotes the interpreter to a real frame
/// stack (`HoltStack` inside the dispatcher) so
/// function calls push and pop without per-call `Vec` allocation.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Index into the bytecode container's function table.
    pub function_id: u32,
    /// Byte offset into the executable function's encoded stream.
    pub pc: u32,
    /// Register window for this frame.
    pub registers: FrameRegisters,
    /// When `Some(reg)`, returning from this frame writes the
    /// completion value into the **caller's** register `reg` and
    /// resumes at the caller's next pc. `<main>` carries `None`
    /// and propagates the value out as the script's completion.
    pub return_register: Option<u16>,
    /// Captured upvalues for this call. Empty for non-closure
    /// frames. Indexed by `Op::LoadUpvalue` / `Op::StoreUpvalue`
    /// operands.
    pub upvalues: UpvalueSpine,
    /// `this` value visible inside the body. `<main>` and free
    /// `Op::Call` invocations both bind `Value::Undefined`
    /// (foundation strict default). Method calls set the receiver,
    /// `Op::CallWithThis` and `Op::CallMethodValue` thread a caller-
    /// provided value, and arrow closures override with their
    /// lexically-captured `this` regardless of the call site.
    pub this_value: Value,
    /// Async-call state: `Some` when this frame belongs to an
    /// `async` function. The result promise was created at call
    /// entry and written into the caller's destination register
    /// **then**; on return / unhandled throw, the dispatcher
    /// settles this promise instead of writing a value to the
    /// caller. `Op::Await` parks the frame off the stack and
    /// re-pushes it from a microtask once the awaited promise
    /// settles. `None` for ordinary (non-async) frames.
    pub async_state: Option<AsyncFrameState>,
    /// Handle into the per-interpreter
    /// [`crate::cold_frame::ColdFramePool`] when this frame has
    /// acquired a cold side record (try handlers, async parking,
    /// pending ToPrimitive/bind/iterator ladders, …). `None` until
    /// the first opcode that needs cold state writes through
    /// [`crate::Interpreter::frame_ensure_cold`].
    pub cold: Option<ColdFrameIdx>,
    /// `Some(gen)` when this frame is the suspended body of an
    /// active generator object. [`otter_bytecode::Op::Yield`]
    /// inspects this slot: if set, the running frame is unspooled
    /// onto the generator's saved-state slot and the dispatcher
    /// returns to the calling `.next()` resume site. `None` for
    /// every other call shape.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-generator-objects>
    pub generator_owner: Option<crate::generator::JsGenerator>,
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
    /// Advance the program counter by `byte_len` bytes. Surfaces
    /// [`VmError::InvalidOperand`] on overflow. The dispatch loop
    /// passes the byte length of the instruction it just executed;
    /// helper opcodes read it indirectly via
    /// [`Interpreter::current_byte_len`].
    pub(crate) fn advance_pc(&mut self, byte_len: u32) -> Result<(), VmError> {
        self.pc = self
            .pc
            .checked_add(byte_len)
            .ok_or(VmError::InvalidOperand)?;
        Ok(())
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
    pub fn for_function(function: &Function) -> Self {
        debug_assert_eq!(
            function.own_upvalue_count, 0,
            "Frame::for_function requires zero own upvalues — use for_function_with_heap or build_upvalues + with_return_upvalues_and_this"
        );
        Self::with_return(function, None)
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
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let upvalues = Self::build_upvalues(heap, function, Self::empty_upvalues())?;
        Ok(Self::with_return_upvalues_and_this(
            function,
            None,
            upvalues,
            Value::undefined(),
        ))
    }

    /// Allocate a frame whose return value should land in the
    /// caller's register `return_register`. Same precondition as
    /// [`Self::for_function`] — zero own upvalues.
    #[must_use]
    pub fn with_return(function: &Function, return_register: Option<u16>) -> Self {
        Self::with_return_upvalues_and_this(
            function,
            return_register,
            Self::empty_upvalues(),
            Value::undefined(),
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
        function: &ExecutableFunction,
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
        function: &ExecutableFunction,
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
            cells.push(crate::alloc_upvalue_with_roots(
                heap,
                Value::undefined(),
                external_visit,
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
    ) -> Self {
        let total = function
            .param_count
            .saturating_add(function.locals)
            .saturating_add(function.scratch) as usize;
        let mut registers: SmallVec<[Value; 8]> = SmallVec::with_capacity(total);
        registers.resize(total, Value::undefined());
        debug_assert!(
            upvalues.len() >= function.own_upvalue_count as usize,
            "frame upvalues must include the function's own cells"
        );
        Self {
            function_id: function.id,
            pc: 0,
            registers: FrameRegisters::Owned(registers),
            return_register,
            upvalues,
            this_value,
            async_state: None,
            cold: None,
            generator_owner: None,
        }
    }

    #[must_use]
    pub(crate) fn with_exec_return_upvalues_and_this(
        function: &ExecutableFunction,
        return_register: Option<u16>,
        upvalues: UpvalueSpine,
        this_value: Value,
    ) -> Self {
        let mut registers: SmallVec<[Value; 8]> =
            SmallVec::with_capacity(function.register_count as usize);
        registers.resize(function.register_count as usize, Value::undefined());
        Self::with_exec_registers(function, return_register, upvalues, this_value, registers)
    }

    /// Same as [`Self::with_exec_return_upvalues_and_this`] but consumes a
    /// caller-supplied register window. The hot call paths draw that window
    /// from [`crate::Interpreter::reg_pool`] (see
    /// [`crate::Interpreter::draw_registers`]) so a recursive / tight-loop call
    /// reuses a spilled buffer instead of allocating one per frame.
    ///
    /// `registers` must already be sized to `function.register_count` and
    /// zero-filled with `Value::undefined()`.
    #[must_use]
    pub(crate) fn with_exec_registers(
        function: &ExecutableFunction,
        return_register: Option<u16>,
        upvalues: UpvalueSpine,
        this_value: Value,
        registers: SmallVec<[Value; 8]>,
    ) -> Self {
        debug_assert_eq!(
            registers.len(),
            function.register_count as usize,
            "register window must match the function's register_count"
        );
        debug_assert!(
            upvalues.len() >= function.own_upvalue_count as usize,
            "frame upvalues must include the function's own cells"
        );
        Self {
            function_id: function.id,
            pc: 0,
            registers: FrameRegisters::Owned(registers),
            return_register,
            upvalues,
            this_value,
            async_state: None,
            cold: None,
            generator_owner: None,
        }
    }

    /// Same as [`Self::with_exec_return_upvalues_and_this`] but backs the
    /// frame's registers with a `Window` into the interpreter's flat register
    /// stack (`ptr`/`base_off` from [`crate::Interpreter::alloc_reg_window`])
    /// instead of an inline-owned buffer. The window is pre-zeroed.
    #[must_use]
    pub(crate) fn with_exec_window(
        function: &ExecutableFunction,
        return_register: Option<u16>,
        upvalues: UpvalueSpine,
        this_value: Value,
        ptr: *mut Value,
        base_off: u32,
    ) -> Self {
        debug_assert!(
            upvalues.len() >= function.own_upvalue_count as usize,
            "frame upvalues must include the function's own cells"
        );
        Self {
            function_id: function.id,
            pc: 0,
            registers: FrameRegisters::Window {
                ptr,
                len: function.register_count,
                base_off,
            },
            return_register,
            upvalues,
            this_value,
            async_state: None,
            cold: None,
            generator_owner: None,
        }
    }

    /// Trace locals, register window, receiver, parked side-channel
    /// values, and nested generator / async state held by this frame.
    pub(crate) fn trace_frame_slots(&self, visitor: &mut SlotVisitor<'_>) {
        for value in self.registers.iter() {
            value.trace_value_slots(visitor);
        }
        for slot in self.upvalues.iter() {
            let p = slot as *const UpvalueCell as *mut RawGc;
            visitor(p);
        }
        self.this_value.trace_value_slots(visitor);
        if let Some(async_state) = &self.async_state {
            async_state.result_promise.trace_value_slots(visitor);
        }
        // Cold-record GC slots (pending_to_primitive / pending_bind_function /
        // pending_iterator_next) are traced separately by the caller through
        // [`crate::cold_frame::ColdFrame::trace_cold_slots`] when
        // `self.cold` is `Some`.
        if let Some(owner) = &self.generator_owner {
            owner.trace_value_slots(visitor);
        }
    }
}
