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
//! # See also
//! - [crate::frame_ops]
//! - [crate::executable]

use smallvec::SmallVec;

use otter_bytecode::Function;
use otter_gc::raw::{RawGc, SlotVisitor};

use crate::{
    ExecutableFunction, JsObject, JsPromiseHandle, UpvalueCell, Value, abstract_ops, alloc_upvalue,
};

/// One call frame. Compact and cache-conscious per foundation
/// plan §M7. Slice 13 promotes the interpreter to a real frame
/// stack (`SmallVec<[Frame; 8]>` inside the dispatcher) so
/// function calls push and pop without per-call `Vec` allocation.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Index into the bytecode container's function table.
    pub function_id: u32,
    /// Current program counter (instruction index, not byte offset).
    pub pc: u32,
    /// Register window for this frame.
    pub registers: SmallVec<[Value; 8]>,
    /// When `Some(reg)`, returning from this frame writes the
    /// completion value into the **caller's** register `reg` and
    /// resumes at the caller's next pc. `<main>` carries `None`
    /// and propagates the value out as the script's completion.
    pub return_register: Option<u16>,
    /// Captured upvalues for this call. Empty for non-closure
    /// frames. Indexed by `Op::LoadUpvalue` / `Op::StoreUpvalue`
    /// operands.
    pub upvalues: std::rc::Rc<[UpvalueCell]>,
    /// `this` value visible inside the body. `<main>` and free
    /// `Op::Call` invocations both bind `Value::Undefined`
    /// (foundation strict default). Method calls set the receiver,
    /// `Op::CallWithThis` and `Op::CallMethodValue` thread a caller-
    /// provided value, and arrow closures override with their
    /// lexically-captured `this` regardless of the call site.
    pub this_value: Value,
    /// Active try-handler stack. Pushed by [`Op::EnterTry`], popped
    /// by [`Op::LeaveTry`] or by an exception unwind landing on a
    /// matching catch / finally. Innermost handler is on top.
    pub handlers: SmallVec<[TryHandler; 4]>,
    /// In-flight exception parked when a throw routed into a
    /// `finally` block. [`Op::EndFinally`] consumes it: `Some` →
    /// re-throw, `None` → fall through. The compiler always emits
    /// `EndFinally` at the close of every finally body, so the
    /// re-throw protocol stays bytecode-visible.
    pub pending_throw: Option<Value>,
    /// Newly-allocated receiver when this frame was entered via
    /// [`Op::New`] (`new C(args)`). On return, [`Interpreter::pop_frame`]
    /// substitutes this object for any non-object return value, so
    /// constructors that don't `return` a replacement still hand the
    /// caller the freshly-built instance.
    pub construct_target: Option<JsObject>,
    /// Trailing arguments past the declared `param_count`. Populated
    /// by the call dispatcher only when the callee declares a rest
    /// parameter (`function f(...rest) { … }`); consumed by
    /// [`otter_bytecode::Op::CollectRest`] which packs them into a
    /// fresh `JsArray`. Always empty for non-rest callees so the
    /// allocation cost is paid only when needed.
    pub rest_args: SmallVec<[Value; 4]>,
    /// `new.target` visible to the active function body. Set only
    /// for frames entered through `[[Construct]]`; ordinary calls
    /// and top-level code observe `undefined`.
    pub new_target: Option<Value>,
    /// Full incoming-argument list captured at call entry. Used by
    /// [`otter_bytecode::Op::CollectArguments`] to materialise an
    /// `arguments`-style array containing every value the caller
    /// supplied — including the named parameters. Populated only
    /// when the callee was compiled with `needs_arguments = true`
    /// so non-arguments-using functions pay no allocation cost.
    pub incoming_args: SmallVec<[Value; 4]>,
    /// Async-call state: `Some` when this frame belongs to an
    /// `async` function. The result promise was created at call
    /// entry and written into the caller's destination register
    /// **then**; on return / unhandled throw, the dispatcher
    /// settles this promise instead of writing a value to the
    /// caller. `Op::Await` parks the frame off the stack and
    /// re-pushes it from a microtask once the awaited promise
    /// settles. `None` for ordinary (non-async) frames.
    pub async_state: Option<AsyncFrameState>,
    /// Source-module URL the running function was compiled from.
    /// Snapshot of [`otter_bytecode::Function::module_url`] at
    /// frame-push time. Read by [`Op::ImportNamespace`] to look
    /// up specifier resolutions in the linker's pre-built
    /// `module_resolutions` table — the caller frame's URL is
    /// the referrer for the import-resolution algorithm.
    ///
    /// Empty string for non-module functions (e.g. the linker's
    /// synthesised `<entry>` driver) — those frames inherit the
    /// caller's URL when invoking module-init functions, but
    /// `Op::ImportNamespace` itself never executes from a
    /// non-module frame in well-formed bytecode.
    pub module_url: std::rc::Rc<str>,
    /// State machine for the in-flight ECMA-262 §7.1.1 `ToPrimitive`
    /// ladder. `Some` while the dispatcher is mid-way through the
    /// `[Symbol.toPrimitive]` / `valueOf` / `toString` chain on a
    /// specific `Op::ToPrimitive` instruction; `None` otherwise.
    /// Set by [`Interpreter::drive_to_primitive`] before pushing a
    /// call frame, cleared once the ladder hands back a primitive
    /// (or exhausts every stage and the dispatcher raises a
    /// `TypeMismatch`).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    pub pending_to_primitive: Option<PendingToPrimitive>,
    /// In-flight ECMA-262 §20.2.3.2
    /// `Function.prototype.bind` metadata collection. `Some`
    /// while `Op::BindFunction` is awaiting an accessor getter for
    /// `target.name` or `target.length`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-function.prototype.bind>
    pub pending_bind_function: Option<PendingBindFunction>,
    /// In-flight ECMA-262 §7.4.3 `GetIterator` over a user object.
    /// `Some` while the dispatcher is awaiting the result of
    /// `obj[@@iterator]()`; the resume step wraps that return
    /// value as [`IteratorState::User`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-getiterator>
    pub pending_get_iterator: Option<PendingGetIterator>,
    /// In-flight ECMA-262 §7.4.5 `IteratorNext` over a user
    /// iterator. `Some` while the dispatcher is awaiting the
    /// result of `iter.next()`; the resume step extracts
    /// `value` / `done` from the returned record.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-iteratornext>
    pub pending_iterator_next: Option<PendingIteratorNext>,
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
    /// invoke it. On resume, validate the result is primitive;
    /// otherwise fall through to [`Self::OrdinaryFirst`].
    SymbolToPrim,
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
    /// Shared empty upvalue slice for plain functions without captured
    /// parent cells.
    pub(crate) fn empty_upvalues() -> std::rc::Rc<[UpvalueCell]> {
        thread_local! {
            static EMPTY_UPVALUES: std::rc::Rc<[UpvalueCell]> =
                std::rc::Rc::from(Vec::<UpvalueCell>::new());
        }

        EMPTY_UPVALUES.with(std::clone::Clone::clone)
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
            Value::Undefined,
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
            Value::Undefined,
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
        parent_upvalues: std::rc::Rc<[UpvalueCell]>,
    ) -> Result<std::rc::Rc<[UpvalueCell]>, otter_gc::OutOfMemory> {
        Self::build_upvalues_for_count(heap, function.own_upvalue_count, parent_upvalues)
    }

    pub(crate) fn build_upvalues_for_exec(
        heap: &mut otter_gc::GcHeap,
        function: &ExecutableFunction,
        parent_upvalues: std::rc::Rc<[UpvalueCell]>,
    ) -> Result<std::rc::Rc<[UpvalueCell]>, otter_gc::OutOfMemory> {
        Self::build_upvalues_for_count(heap, function.own_upvalue_count, parent_upvalues)
    }

    fn build_upvalues_for_count(
        heap: &mut otter_gc::GcHeap,
        own_upvalue_count: u16,
        parent_upvalues: std::rc::Rc<[UpvalueCell]>,
    ) -> Result<std::rc::Rc<[UpvalueCell]>, otter_gc::OutOfMemory> {
        let own = own_upvalue_count as usize;
        if own == 0 {
            return Ok(parent_upvalues);
        }
        let mut cells: Vec<UpvalueCell> = Vec::with_capacity(own + parent_upvalues.len());
        for _ in 0..own {
            cells.push(alloc_upvalue(heap, Value::Undefined)?);
        }
        cells.extend(parent_upvalues.iter().copied());
        Ok(std::rc::Rc::from(cells))
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
        upvalues: std::rc::Rc<[UpvalueCell]>,
        this_value: Value,
    ) -> Self {
        let total = function
            .param_count
            .saturating_add(function.locals)
            .saturating_add(function.scratch) as usize;
        let mut registers: SmallVec<[Value; 8]> = SmallVec::with_capacity(total);
        registers.resize(total, Value::Undefined);
        debug_assert!(
            upvalues.len() >= function.own_upvalue_count as usize,
            "frame upvalues must include the function's own cells"
        );
        Self {
            function_id: function.id,
            pc: 0,
            registers,
            return_register,
            upvalues,
            this_value,
            handlers: SmallVec::new(),
            pending_throw: None,
            construct_target: None,
            rest_args: SmallVec::new(),
            new_target: None,
            incoming_args: SmallVec::new(),
            async_state: None,
            module_url: std::rc::Rc::from(function.module_url.as_str()),
            pending_to_primitive: None,
            pending_bind_function: None,
            pending_get_iterator: None,
            pending_iterator_next: None,
            generator_owner: None,
        }
    }

    #[must_use]
    pub(crate) fn with_exec_return_upvalues_and_this(
        function: &ExecutableFunction,
        return_register: Option<u16>,
        upvalues: std::rc::Rc<[UpvalueCell]>,
        this_value: Value,
    ) -> Self {
        let mut registers: SmallVec<[Value; 8]> =
            SmallVec::with_capacity(function.register_count as usize);
        registers.resize(function.register_count as usize, Value::Undefined);
        debug_assert!(
            upvalues.len() >= function.own_upvalue_count as usize,
            "frame upvalues must include the function's own cells"
        );
        Self {
            function_id: function.id,
            pc: 0,
            registers,
            return_register,
            upvalues,
            this_value,
            handlers: SmallVec::new(),
            pending_throw: None,
            construct_target: None,
            rest_args: SmallVec::new(),
            new_target: None,
            incoming_args: SmallVec::new(),
            async_state: None,
            module_url: std::rc::Rc::from(function.module_url.as_ref()),
            pending_to_primitive: None,
            pending_bind_function: None,
            pending_get_iterator: None,
            pending_iterator_next: None,
            generator_owner: None,
        }
    }

    /// Trace locals, register window, receiver, parked side-channel
    /// values, and nested generator / async state held by this frame.
    pub(crate) fn trace_frame_slots(&self, visitor: &mut SlotVisitor<'_>) {
        for value in &self.registers {
            value.trace_value_slots(visitor);
        }
        for slot in self.upvalues.iter() {
            let p = slot as *const UpvalueCell as *mut RawGc;
            visitor(p);
        }
        self.this_value.trace_value_slots(visitor);
        for value in &self.rest_args {
            value.trace_value_slots(visitor);
        }
        if let Some(value) = &self.new_target {
            value.trace_value_slots(visitor);
        }
        for value in &self.incoming_args {
            value.trace_value_slots(visitor);
        }
        if let Some(value) = &self.pending_throw {
            value.trace_value_slots(visitor);
        }
        if let Some(obj) = &self.construct_target {
            let p = obj as *const JsObject as *mut RawGc;
            visitor(p);
        }
        if let Some(async_state) = &self.async_state {
            async_state.result_promise.trace_value_slots(visitor);
        }
        if let Some(pending) = &self.pending_to_primitive {
            pending.obj.trace_value_slots(visitor);
        }
        if let Some(pending) = &self.pending_bind_function {
            pending.target.trace_value_slots(visitor);
            pending.bound_this.trace_value_slots(visitor);
            for arg in &pending.bound_args {
                arg.trace_value_slots(visitor);
            }
            if let Some(name) = &pending.target_name {
                name.trace_value_slots(visitor);
            }
        }
        if let Some(pending) = &self.pending_iterator_next {
            pending.iterator.trace_value_slots(visitor);
        }
        if let Some(owner) = &self.generator_owner {
            owner.trace_value_slots(visitor);
        }
    }
}
