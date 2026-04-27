//! Interpreter and value model for the new Otter engine.
//!
//! Foundation phase is **interpreter-only** (foundation plan §15).
//! No JIT, no GC integration yet — values for the harness slice are
//! plain `Value::Undefined`. Slice tasks `09`+ extend the value
//! model.
//!
//! # Contents
//! - [`Value`] — opaque runtime value (foundation: only `Undefined`).
//! - [`Frame`] — compact call frame.
//! - [`Interpreter`] — match-based dispatch loop over
//!   [`otter_bytecode::BytecodeModule`].
//! - [`InterruptFlag`] — atomic flag observed at back-edges; cheap.
//! - [`VmError`] — the small enum of runtime errors the interpreter
//!   can raise.
//!
//! # Invariants
//! - One thread, one [`Interpreter`]. `Send`/`Sync` are not
//!   implemented.
//! - The dispatch loop polls [`InterruptFlag`] before every
//!   instruction in the harness slice (back-edges arrive in slice
//!   `12`).
//!
//! # See also
//! - [`docs/new-engine/adr/0003-public-api-and-cli.md`](
//!     ../../../docs/new-engine/adr/0003-public-api-and-cli.md
//!   )
//! - [`docs/new-engine/specs/bytecode-dump-disasm-trace.md`](
//!     ../../../docs/new-engine/specs/bytecode-dump-disasm-trace.md
//!   )

pub mod array;
pub mod array_prototype;
pub mod bigint;
pub mod intrinsics;
pub mod math;
pub mod number;
pub mod object;
pub mod string;
pub mod string_prototype;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use otter_bytecode::{BytecodeModule, Constant, Function, Op, Operand};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::intrinsics::{IntrinsicArgs, IntrinsicError};

pub use array::JsArray;
pub use number::{NumberValue, NumericOrdering};
pub use object::JsObject;
pub use string::{JsString, MAX_ROPE_DEPTH, StringError, StringHeap, StringRepr};

/// Foundation runtime value.
///
/// Slice 09 introduced `String`; slice 11 adds `Number` and
/// `Boolean`. Later slices add `Null`, `Object`, etc. The foundation
/// `Value` is intentionally **not** `Copy` — `JsString` owns an
/// `Arc` payload.
#[derive(Debug, Clone)]
pub enum Value {
    /// JS `undefined`.
    Undefined,
    /// JS `null`.
    Null,
    /// JS `true` / `false`.
    Boolean(bool),
    /// JS Number (smi + double; see [`NumberValue`]).
    Number(NumberValue),
    /// JS BigInt — arbitrary-precision integer. Distinct from
    /// `Number`; mixing the two through arithmetic is a spec
    /// `TypeError`. See [`bigint::BigIntValue`].
    BigInt(bigint::BigIntValue),
    /// JS string. Storage is WTF-16 with cons / sliced ropes; see
    /// [`JsString`].
    String(JsString),
    /// JS function. Foundation slice 13: a closure-less reference
    /// to a [`otter_bytecode::Function`] in the loaded module.
    /// Real closures (captured upvalues) arrive in a later slice.
    Function {
        /// Index into [`otter_bytecode::BytecodeModule::functions`].
        function_id: u32,
    },
    /// JS object — heap-shared, mutable. See [`JsObject`].
    Object(JsObject),
    /// JS array — dense, heap-shared. See [`JsArray`].
    Array(JsArray),
    /// Closure — function with captured upvalues. See
    /// [`UpvalueCell`].
    Closure {
        /// Index into [`otter_bytecode::BytecodeModule::functions`].
        function_id: u32,
        /// Captured cells, in declaration order. The compiler emits
        /// `MakeFunction` for closure-less, non-arrow functions and
        /// reserves `MakeClosure` for the capture path and for all
        /// arrow expressions.
        upvalues: std::rc::Rc<[UpvalueCell]>,
        /// `Some(this)` for arrow closures: the lexically-captured
        /// receiver always wins over whatever the call site passes.
        /// `None` for non-arrow closures, which take their `this`
        /// from the call site.
        bound_this: Option<Box<Value>>,
    },
    /// Result of `Function.prototype.bind(thisArg, ...prefix)`. When
    /// invoked, forwards to `target` with `this = bound_this` and
    /// `prefix ++ call_args` as the argument list. Cheap to clone:
    /// the wrapper is `Rc`-shared.
    BoundFunction(std::rc::Rc<BoundFunction>),
    /// Internal iterator state, produced by [`otter_bytecode::Op::GetIterator`]
    /// and driven by [`otter_bytecode::Op::IteratorNext`]. Until
    /// task 37 adds real `Symbol.iterator` lookup, the foundation
    /// models iterators out-of-band as a dedicated value variant
    /// — they are not addressable via `o[@@iterator]` from user
    /// code.
    Iterator(std::rc::Rc<std::cell::RefCell<IteratorState>>),
    /// Class value: the result of evaluating a `class` declaration
    /// or expression. Wraps the underlying constructor callable,
    /// the prototype object that fresh instances inherit from, and
    /// a static-side object that holds class statics (and chains
    /// through `extends`). The dispatcher unwraps a class to its
    /// inner constructor for `Op::Call` / `Op::New`, but treats
    /// `LoadProperty` / `StoreProperty` against the class as
    /// operations on the static side (with `"prototype"` aliased
    /// to the prototype object directly).
    ClassConstructor(std::rc::Rc<ClassConstructor>),
}

/// Storage for [`Value::ClassConstructor`]. Cloned by handle so
/// passing a class through registers stays cheap; the wrapper is
/// `Rc`-shared and the inner objects are themselves heap-shared.
#[derive(Debug)]
pub struct ClassConstructor {
    /// The actual callable (a `Value::Function` or
    /// `Value::Closure`) the runtime invokes for `new C(...)` or
    /// `super(...)`. Constructed by the compiler's class-lowering
    /// pass.
    pub ctor: Value,
    /// `C.prototype` — every instance built by `new C(...)`
    /// inherits from this object, and instance methods live here.
    pub prototype: JsObject,
    /// Static side: own static methods/properties live here, and
    /// when `class D extends C` the static object's
    /// `[[Prototype]]` chains to `C`'s static object so static
    /// inheritance just falls out of the existing prototype walker.
    pub statics: JsObject,
}

/// Foundation iterator-state machine. Each variant carries the
/// minimum information needed to advance one step at a time. Once
/// the iterator reports `done`, subsequent calls keep returning
/// `done = true` with `value = undefined` (per spec §7.4.2 step 6).
#[derive(Debug)]
pub enum IteratorState {
    /// Walks `array`'s dense storage in insertion order.
    Array {
        /// Backing array — held by `JsArray`'s internal `Rc` so
        /// mutation through the original handle is observable.
        array: JsArray,
        /// Next element index to read. Compared against the
        /// array's `len()` at every step so resizing the array
        /// during iteration is observed correctly.
        index: usize,
    },
    /// Walks `string`'s WTF-16 code units, yielding one-unit
    /// strings. Surrogate pairs split (matches `String[@@iterator]`
    /// only loosely; full code-point iteration arrives with task
    /// 30's string completion).
    String {
        /// Backing string.
        string: JsString,
        /// Next code-unit index.
        index: u32,
    },
    /// Permanently exhausted iterator — every step returns
    /// `done = true`. The runtime transitions any iterator to this
    /// state once it observes `done`, so re-driving an exhausted
    /// iterator is a no-op rather than a re-iteration.
    Exhausted,
}

/// Storage for `Value::BoundFunction`. Constructed by the
/// `Op::BindFunction` opcode and consumed by every call dispatch
/// path (`Op::Call`, `Op::CallWithThis`, `Op::CallMethodValue`).
#[derive(Debug, Clone)]
pub struct BoundFunction {
    /// Underlying callable. Foundation slice keeps this as a
    /// `Value`; chained `bind` flattens by re-wrapping at call
    /// time without unbounded recursion (one hop per layer).
    pub target: Value,
    /// The `this` value the bound call receives. Overrides any
    /// receiver the caller supplies.
    pub bound_this: Value,
    /// Arguments prepended to the caller's argument list at every
    /// invocation. Stored inline up to four entries to keep the
    /// usual `f.bind(t, a, b)` shape off the heap.
    pub bound_args: SmallVec<[Value; 4]>,
}

/// One captured-variable cell. Cloning shares the same heap slot
/// so multiple closures + the original outer scope all see
/// mutations through it.
///
/// Inside the foundation slice the cell stores a plain `Value`
/// behind `Rc<RefCell<>>` — once a real GC ships, this becomes a
/// GC handle.
#[derive(Debug, Clone)]
pub struct UpvalueCell(std::rc::Rc<std::cell::RefCell<Value>>);

impl UpvalueCell {
    /// Construct a fresh cell pre-populated with `value`.
    #[must_use]
    pub fn new(value: Value) -> Self {
        Self(std::rc::Rc::new(std::cell::RefCell::new(value)))
    }

    /// Read the captured value (clones the payload).
    #[must_use]
    pub fn get(&self) -> Value {
        self.0.borrow().clone()
    }

    /// Write a new value. Visible through every clone of this cell.
    pub fn set(&self, value: Value) {
        *self.0.borrow_mut() = value;
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        std::rc::Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Value {
    /// Convenience: shared empty-string constant. Allocates only on
    /// first call per heap.
    pub fn empty_string(heap: &StringHeap) -> Result<Self, StringError> {
        Ok(Self::String(JsString::empty(heap)?))
    }

    /// Render the value as a debug-style string suitable for CLI
    /// preview output (e.g., `otter -p '"abc"'`).
    #[must_use]
    pub fn display_string(&self) -> String {
        match self {
            Value::Undefined => "undefined".to_string(),
            Value::Null => "null".to_string(),
            Value::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Number(n) => n.to_display_string(),
            // BigInt rendering matches `BigInt.prototype.toString`:
            // decimal digits, no `n` suffix.
            Value::BigInt(b) => b.to_decimal_string(),
            Value::String(s) => s.to_lossy_string(),
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                format!("[Function #{function_id}]")
            }
            Value::BoundFunction(b) => format!("[BoundFunction → {}]", b.target.display_string()),
            Value::Iterator(_) => "[object Iterator]".to_string(),
            Value::ClassConstructor(_) => "[class]".to_string(),
            Value::Object(_) => "[object Object]".to_string(),
            Value::Array(a) => {
                let body = a.borrow_body();
                let parts: Vec<String> = body.iter().map(Value::display_string).collect();
                parts.join(",")
            }
        }
    }

    /// Spec [`ToBoolean`](https://tc39.es/ecma262/#sec-toboolean)
    /// for the foundation subset.
    #[must_use]
    pub fn to_boolean(&self) -> bool {
        match self {
            Value::Undefined | Value::Null => false,
            Value::Boolean(b) => *b,
            Value::Number(n) => {
                if n.is_nan() {
                    false
                } else {
                    n.as_f64() != 0.0
                }
            }
            // Spec ToBoolean(BigInt): false iff zero.
            Value::BigInt(b) => !b.as_inner().sign().eq(&num_bigint::Sign::NoSign),
            Value::String(s) => !s.is_empty(),
            Value::Function { .. }
            | Value::Closure { .. }
            | Value::BoundFunction(_)
            | Value::Object(_)
            | Value::Array(_)
            | Value::Iterator(_)
            | Value::ClassConstructor(_) => true,
        }
    }

    /// Spec "is nullish" (`null` or `undefined`).
    #[must_use]
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }

    /// Borrow as a [`JsString`] when the value is a string.
    #[must_use]
    pub fn as_string(&self) -> Option<&JsString> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// Borrow as a [`NumberValue`] when the value is numeric.
    #[must_use]
    pub fn as_number(&self) -> Option<NumberValue> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Borrow as a `bool` when the value is a boolean.
    #[must_use]
    pub fn as_boolean(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    /// Construct a string value from in-memory text. Convenience
    /// for tests and the compiler's literal table.
    ///
    /// # Errors
    /// See [`JsString::from_str`].
    pub fn from_str(s: &str, heap: &StringHeap) -> Result<Self, StringError> {
        Ok(Self::String(JsString::from_str(s, heap)?))
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Undefined, Value::Undefined) => true,
            (Value::Null, Value::Null) => true,
            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            (Value::Number(a), Value::Number(b)) => number::equals(*a, *b),
            // Strict equality across Number / BigInt is always
            // `false` per spec; the wildcard arm below handles
            // the cross-kind case.
            (Value::BigInt(a), Value::BigInt(b)) => a == b,
            (Value::String(a), Value::String(b)) => a.equals(b),
            (Value::Object(a), Value::Object(b)) => a.ptr_eq(b),
            (Value::Array(a), Value::Array(b)) => a.ptr_eq(b),
            (Value::Function { function_id: a }, Value::Function { function_id: b }) => a == b,
            (
                Value::Closure {
                    function_id: a,
                    upvalues: ua,
                    ..
                },
                Value::Closure {
                    function_id: b,
                    upvalues: ub,
                    ..
                },
            ) => a == b && std::rc::Rc::ptr_eq(ua, ub),
            (Value::BoundFunction(a), Value::BoundFunction(b)) => std::rc::Rc::ptr_eq(a, b),
            (Value::Iterator(a), Value::Iterator(b)) => std::rc::Rc::ptr_eq(a, b),
            (Value::ClassConstructor(a), Value::ClassConstructor(b)) => std::rc::Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for Value {}

/// Cooperative cancellation flag.
///
/// Cheap, cloneable, `Send + Sync`. The interpreter polls this flag
/// before each instruction. An interrupt request converts into
/// [`VmError::Interrupted`] at the next checkpoint.
#[derive(Debug, Default, Clone)]
pub struct InterruptFlag(Arc<AtomicBool>);

impl InterruptFlag {
    /// Construct a fresh, un-tripped flag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the flag from any thread.
    pub fn interrupt(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Check the flag without resetting it.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Reset the flag.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }
}

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
    /// Allocate a frame for `function`. Registers are pre-filled
    /// with `Value::Undefined`. Used for `<main>` (return register
    /// = `None`, `this` = `undefined`).
    #[must_use]
    pub fn for_function(function: &Function) -> Self {
        Self::with_return(function, None)
    }

    /// Allocate a frame whose return value should land in the
    /// caller's register `return_register`.
    #[must_use]
    pub fn with_return(function: &Function, return_register: Option<u16>) -> Self {
        Self::with_return_and_upvalues(function, return_register, std::rc::Rc::from(Vec::new()))
    }

    /// Allocate a frame and bind captured upvalues. `this` is left
    /// at the foundation default (`Value::Undefined`); call sites
    /// that need a non-default receiver use
    /// [`Self::with_return_upvalues_and_this`].
    #[must_use]
    pub fn with_return_and_upvalues(
        function: &Function,
        return_register: Option<u16>,
        parent_upvalues: std::rc::Rc<[UpvalueCell]>,
    ) -> Self {
        Self::with_return_upvalues_and_this(
            function,
            return_register,
            parent_upvalues,
            Value::Undefined,
        )
    }

    /// Full constructor used by call sites that need to bind a
    /// non-default `this`. The function's own captured locals are
    /// appended after the inherited parent upvalues — see
    /// [`Op::MakeClosure`](otter_bytecode::Op::MakeClosure) for the
    /// layout.
    #[must_use]
    pub fn with_return_upvalues_and_this(
        function: &Function,
        return_register: Option<u16>,
        parent_upvalues: std::rc::Rc<[UpvalueCell]>,
        this_value: Value,
    ) -> Self {
        let total = function
            .param_count
            .saturating_add(function.locals)
            .saturating_add(function.scratch) as usize;
        let mut registers: SmallVec<[Value; 8]> = SmallVec::with_capacity(total);
        registers.resize(total, Value::Undefined);
        let own = function.own_upvalue_count as usize;
        // Layout: [own_caps..., parent_caps...]. Own slots come
        // first so the compiler can assign stable indices `0..own`
        // at declaration time before knowing how many parent
        // captures will be added during the body's compilation.
        let upvalues: std::rc::Rc<[UpvalueCell]> = if own == 0 {
            parent_upvalues
        } else {
            let mut cells: Vec<UpvalueCell> = Vec::with_capacity(own + parent_upvalues.len());
            for _ in 0..own {
                cells.push(UpvalueCell::new(Value::Undefined));
            }
            cells.extend(parent_upvalues.iter().cloned());
            std::rc::Rc::from(cells)
        };
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
        }
    }
}

/// Runtime errors raised by the interpreter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum VmError {
    /// The program counter walked off the end of `code` without a
    /// `RETURN`. Indicates a compiler bug.
    MissingReturn,
    /// An operand index was out of range. Indicates a compiler bug
    /// or a malformed bytecode dump.
    InvalidOperand,
    /// An operand had the wrong type for its opcode (e.g.,
    /// `STRING_CONCAT` on a non-string register). Indicates a
    /// compiler bug at this slice.
    TypeMismatch,
    /// String allocation failed because the heap cap was hit.
    OutOfMemory {
        /// Bytes the allocation requested.
        requested_bytes: u64,
        /// Heap cap (`0` = unlimited).
        heap_limit_bytes: u64,
    },
    /// `InterruptFlag` was tripped before the next checkpoint.
    Interrupted,
    /// `CALL_STRING_METHOD` referenced a method name not in
    /// [`string_prototype::STRING_PROTOTYPE_TABLE`].
    UnknownIntrinsic {
        /// Method name as it appeared in the constant pool.
        name: String,
    },
    /// A `let`/`const` binding was read before its initializer ran
    /// (Temporal Dead Zone).
    TemporalDeadZone {
        /// Compiler-assigned local index.
        local_index: u32,
    },
    /// JS call-stack depth exceeded the configured limit. Catchable
    /// per foundation plan §M7 ("stack-depth limit returns a
    /// catchable JS error").
    StackOverflow {
        /// Maximum depth that was about to be exceeded.
        limit: u32,
    },
    /// Tried to call a value that is not callable.
    NotCallable,
    /// A user `throw` (or a re-throw from `finally`) walked the
    /// entire frame stack without finding a matching handler. The
    /// payload is the JS value that was thrown, rendered for
    /// diagnostics through [`Value::display_string`]; the runtime
    /// surfaces this as `OtterError::Runtime { code = "UNCAUGHT" }`.
    Uncaught {
        /// Display rendering of the thrown value.
        value: String,
    },
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::MissingReturn => write!(f, "function did not RETURN"),
            VmError::InvalidOperand => write!(f, "invalid operand"),
            VmError::TypeMismatch => write!(f, "operand type mismatch"),
            VmError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => write!(
                f,
                "out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}"
            ),
            VmError::Interrupted => write!(f, "interrupted"),
            VmError::UnknownIntrinsic { name } => write!(f, "unknown intrinsic method `{name}`"),
            VmError::TemporalDeadZone { local_index } => {
                write!(f, "cannot access local {local_index} before initialization")
            }
            VmError::StackOverflow { limit } => {
                write!(f, "maximum call stack size exceeded (limit {limit})")
            }
            VmError::NotCallable => write!(f, "value is not a function"),
            VmError::Uncaught { value } => write!(f, "uncaught exception: {value}"),
        }
    }
}

impl std::error::Error for VmError {}

impl From<StringError> for VmError {
    fn from(err: StringError) -> Self {
        match err {
            StringError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => VmError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            },
        }
    }
}

/// Default JS call-stack depth limit. Catchable via
/// [`VmError::StackOverflow`].
pub const DEFAULT_MAX_STACK_DEPTH: u32 = 1024;

/// Re-export of the bytecode-defined sentinel for "this try block
/// has no catch / finally clause". Kept on the VM surface so
/// embedders that want to hand-build EnterTry operands have one
/// import path for the runtime semantics.
pub use otter_bytecode::NO_HANDLER_OFFSET;

/// One stack-frame snapshot captured at the moment an error is
/// raised. Foundation slice 16 ships this — task 24 (exceptions)
/// reuses it for catchable error frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackFrameSnapshot {
    /// Function name; `<main>` for the script entry,
    /// `<arrow>`/`<anonymous>` for function expressions.
    pub function_name: String,
    /// Module specifier the function was compiled from.
    pub module: String,
    /// Source span of the failing instruction (byte offsets).
    pub span: (u32, u32),
}

/// Result type returned by [`Interpreter::run`] on failure: the
/// underlying [`VmError`] plus a snapshot of the live frame stack
/// at the moment the error was raised. Caller-level translation
/// (e.g., `otter-runtime::map_vm_error`) propagates `frames` into
/// `Diagnostic.frames`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    /// Underlying error.
    pub error: VmError,
    /// Top-of-stack first; element zero is the failing function.
    pub frames: Vec<StackFrameSnapshot>,
}

impl RunError {
    /// Convenience constructor for the no-frames case (e.g., setup
    /// errors before any frame exists).
    #[must_use]
    pub fn bare(error: VmError) -> Self {
        Self {
            error,
            frames: Vec::new(),
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for RunError {}

/// Match-based dispatch loop. The harness baseline; slice tasks may
/// later switch to threaded dispatch after benchmark-driven review
/// (foundation plan §"Interpreter requirements").
#[derive(Debug)]
pub struct Interpreter {
    interrupt: InterruptFlag,
    string_heap: Arc<StringHeap>,
    max_stack_depth: u32,
}

impl Interpreter {
    /// Construct a fresh interpreter with its own interrupt flag,
    /// a no-cap string heap, and the default stack-depth limit.
    #[must_use]
    pub fn new() -> Self {
        Self {
            interrupt: InterruptFlag::new(),
            string_heap: Arc::new(StringHeap::default()),
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
        }
    }

    /// Construct an interpreter with a string heap cap (`0` =
    /// unlimited).
    #[must_use]
    pub fn with_string_heap_cap(cap_bytes: u64) -> Self {
        Self {
            interrupt: InterruptFlag::new(),
            string_heap: Arc::new(StringHeap::with_cap(cap_bytes)),
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
        }
    }

    /// Override the stack-depth limit. `0` is treated as the
    /// configured default (foundation slice rejects an explicit
    /// `0` limit at the `RuntimeBuilder` boundary, so this
    /// fall-through is defensive).
    pub fn set_max_stack_depth(&mut self, depth: u32) {
        self.max_stack_depth = if depth == 0 {
            DEFAULT_MAX_STACK_DEPTH
        } else {
            depth
        };
    }

    /// Cloneable handle for cooperative cancellation.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptFlag {
        self.interrupt.clone()
    }

    /// Borrow the string heap accountant. Tests use this to assert
    /// counter behavior on rejected allocations.
    #[must_use]
    pub fn string_heap(&self) -> &StringHeap {
        &self.string_heap
    }

    /// Execute `<main>` of `module` and return its completion value.
    ///
    /// # Errors
    /// Returns [`RunError`] (a `VmError` plus a stack-frame
    /// snapshot) on bytecode malformation, type mismatch, OOM,
    /// interrupt, or stack overflow.
    pub fn run(&self, module: &BytecodeModule) -> Result<Value, RunError> {
        match self.run_inner(module) {
            Ok(v) => Ok(v),
            Err((error, frames)) => Err(RunError { error, frames }),
        }
    }

    /// Internal driver. Pulls the snapshot capture out of the
    /// dispatch loop so the hot path remains allocation-free; the
    /// snapshot is built only when a `VmError` actually escapes.
    fn run_inner(
        &self,
        module: &BytecodeModule,
    ) -> Result<Value, (VmError, Vec<StackFrameSnapshot>)> {
        let main = module.main();
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(main));

        match self.dispatch_loop(module, &mut stack) {
            Ok(value) => Ok(value),
            Err(err) => {
                let frames = snapshot_frames(module, &stack);
                Err((err, frames))
            }
        }
    }

    fn dispatch_loop(
        &self,
        module: &BytecodeModule,
        stack: &mut SmallVec<[Frame; 8]>,
    ) -> Result<Value, VmError> {
        loop {
            if self.interrupt.is_set() {
                return Err(VmError::Interrupted);
            }
            let top_idx = stack.len() - 1;
            let function_id = stack[top_idx].function_id;
            let function = module
                .functions
                .get(function_id as usize)
                .ok_or(VmError::InvalidOperand)?;
            let pc = stack[top_idx].pc;
            let instr = function
                .code
                .get(pc as usize)
                .ok_or(VmError::MissingReturn)?;
            let op = instr.op;
            let operands = instr.operands.clone();

            // Stack-modifying opcodes go first so we don't hold a
            // `&mut Frame` borrow while pushing / popping.
            match op {
                Op::ReturnValue | Op::Return => {
                    let src = register_operand(operands.first())?;
                    let value = stack[top_idx]
                        .registers
                        .get(src as usize)
                        .cloned()
                        .ok_or(VmError::InvalidOperand)?;
                    if let Some(popped) = self.pop_frame(stack, value)? {
                        return Ok(popped);
                    }
                    continue;
                }
                Op::ReturnUndefined => {
                    if let Some(popped) = self.pop_frame(stack, Value::Undefined)? {
                        return Ok(popped);
                    }
                    continue;
                }
                Op::Call => {
                    self.do_call(stack, module, &operands)?;
                    continue;
                }
                Op::CallWithThis => {
                    self.do_call_with_this(stack, module, &operands)?;
                    continue;
                }
                Op::CallMethodValue => {
                    self.do_call_method_value(stack, module, &operands)?;
                    continue;
                }
                Op::CallSpread => {
                    self.do_call_spread(stack, module, &operands)?;
                    continue;
                }
                Op::New => {
                    self.do_construct(stack, module, &operands)?;
                    continue;
                }
                Op::Throw => {
                    let src = register_operand(operands.first())?;
                    let value = stack[top_idx]
                        .registers
                        .get(src as usize)
                        .cloned()
                        .ok_or(VmError::InvalidOperand)?;
                    Self::unwind_throw(stack, value)?;
                    continue;
                }
                Op::EndFinally => {
                    if let Some(value) = stack[top_idx].pending_throw.take() {
                        Self::unwind_throw(stack, value)?;
                    } else {
                        stack[top_idx].pc = stack[top_idx]
                            .pc
                            .checked_add(1)
                            .ok_or(VmError::InvalidOperand)?;
                    }
                    continue;
                }
                _ => {}
            }

            let frame = &mut stack[top_idx];
            match op {
                Op::Nop => {
                    frame.pc += 1;
                }
                Op::LoadUndefined => {
                    let dst = register_operand(operands.first())?;
                    write_register(frame, dst, Value::Undefined)?;
                    frame.pc += 1;
                }
                Op::Return
                | Op::ReturnValue
                | Op::ReturnUndefined
                | Op::Call
                | Op::CallWithThis
                | Op::CallMethodValue
                | Op::CallSpread
                | Op::New
                | Op::Throw
                | Op::EndFinally => {
                    unreachable!("stack-modifying ops handled earlier in this loop")
                }
                Op::MakeFunction => {
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let function_id = match module.constants.get(idx as usize) {
                        Some(Constant::FunctionId { index }) => *index,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(frame, dst, Value::Function { function_id })?;
                    frame.pc += 1;
                }
                Op::MakeClosure => {
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let function_id = match module.constants.get(idx as usize) {
                        Some(Constant::FunctionId { index }) => *index,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let count = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let mut cells: Vec<UpvalueCell> = Vec::with_capacity(count);
                    for i in 0..count {
                        let parent_idx = match operands.get(3 + i) {
                            Some(&Operand::Imm32(n)) if n >= 0 => n as usize,
                            _ => return Err(VmError::InvalidOperand),
                        };
                        let cell = frame
                            .upvalues
                            .get(parent_idx)
                            .cloned()
                            .ok_or(VmError::InvalidOperand)?;
                        cells.push(cell);
                    }
                    let upvalues: std::rc::Rc<[UpvalueCell]> = std::rc::Rc::from(cells);
                    // Arrow-closure receivers are bound lexically:
                    // every later invocation ignores the call site
                    // and uses the enclosing frame's `this`.
                    let is_arrow = module
                        .functions
                        .get(function_id as usize)
                        .map(|f| f.is_arrow)
                        .unwrap_or(false);
                    let bound_this = if is_arrow {
                        Some(Box::new(frame.this_value.clone()))
                    } else {
                        None
                    };
                    write_register(
                        frame,
                        dst,
                        Value::Closure {
                            function_id,
                            upvalues,
                            bound_this,
                        },
                    )?;
                    frame.pc += 1;
                }
                Op::LoadUpvalue => {
                    let dst = register_operand(operands.first())?;
                    let idx = imm32_operand(operands.get(1))?;
                    if idx < 0 {
                        return Err(VmError::InvalidOperand);
                    }
                    let value = frame
                        .upvalues
                        .get(idx as usize)
                        .ok_or(VmError::InvalidOperand)?
                        .get();
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::StoreUpvalue => {
                    let src = register_operand(operands.first())?;
                    let idx = imm32_operand(operands.get(1))?;
                    if idx < 0 {
                        return Err(VmError::InvalidOperand);
                    }
                    let value = read_register(frame, src)?.clone();
                    frame
                        .upvalues
                        .get(idx as usize)
                        .ok_or(VmError::InvalidOperand)?
                        .set(value);
                    frame.pc += 1;
                }
                Op::LoadString => {
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let units = match module.constants.get(idx as usize) {
                        Some(otter_bytecode::Constant::String { utf16 }) => utf16.as_slice(),
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let s = JsString::from_utf16_units(units, &self.string_heap)?;
                    write_register(frame, dst, Value::String(s))?;
                    frame.pc += 1;
                }
                Op::LoadLength => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let s = read_register(frame, src)?
                        .as_string()
                        .ok_or(VmError::TypeMismatch)?;
                    let len = NumberValue::from_i32(s.len() as i32);
                    write_register(frame, dst, Value::Number(len))?;
                    frame.pc += 1;
                }
                Op::LoadNumber => {
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let value = match module.constants.get(idx as usize) {
                        Some(Constant::Number { bits }) => {
                            NumberValue::from_f64(f64::from_bits(*bits))
                        }
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(frame, dst, Value::Number(value))?;
                    frame.pc += 1;
                }
                Op::LoadInt32 => {
                    let dst = register_operand(operands.first())?;
                    let imm = match operands.get(1) {
                        Some(&Operand::Imm32(v)) => v,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(frame, dst, Value::Number(NumberValue::Smi(imm)))?;
                    frame.pc += 1;
                }
                Op::LoadBigInt => {
                    let dst = register_operand(operands.first())?;
                    let idx = const_operand(operands.get(1))?;
                    let value = match module.constants.get(idx as usize) {
                        Some(Constant::BigInt { decimal }) => {
                            bigint::BigIntValue::from_decimal(decimal)
                                .ok_or(VmError::InvalidOperand)?
                        }
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(frame, dst, Value::BigInt(value))?;
                    frame.pc += 1;
                }
                Op::LoadTrue => {
                    let dst = register_operand(operands.first())?;
                    write_register(frame, dst, Value::Boolean(true))?;
                    frame.pc += 1;
                }
                Op::LoadFalse => {
                    let dst = register_operand(operands.first())?;
                    write_register(frame, dst, Value::Boolean(false))?;
                    frame.pc += 1;
                }
                Op::LoadNull => {
                    let dst = register_operand(operands.first())?;
                    write_register(frame, dst, Value::Null)?;
                    frame.pc += 1;
                }
                Op::LogicalNot => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let truthy = read_register(frame, src)?.to_boolean();
                    write_register(frame, dst, Value::Boolean(!truthy))?;
                    frame.pc += 1;
                }
                Op::ToBoolean => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let truthy = read_register(frame, src)?.to_boolean();
                    write_register(frame, dst, Value::Boolean(truthy))?;
                    frame.pc += 1;
                }
                Op::Jump => {
                    let offset = imm32_operand(operands.first())?;
                    apply_branch(frame, offset, &self.interrupt)?;
                }
                Op::JumpIfTrue => {
                    let offset = imm32_operand(operands.first())?;
                    let cond = register_operand(operands.get(1))?;
                    if read_register(frame, cond)?.to_boolean() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                }
                Op::JumpIfFalse => {
                    let offset = imm32_operand(operands.first())?;
                    let cond = register_operand(operands.get(1))?;
                    if !read_register(frame, cond)?.to_boolean() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                }
                Op::JumpIfNullish => {
                    let offset = imm32_operand(operands.first())?;
                    let cond = register_operand(operands.get(1))?;
                    if read_register(frame, cond)?.is_nullish() {
                        apply_branch(frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                }
                Op::LoadLocal => {
                    let dst = register_operand(operands.first())?;
                    let idx = imm32_operand(operands.get(1))?;
                    let value = read_register(frame, idx as u16)?.clone();
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::StoreLocal => {
                    let src = register_operand(operands.first())?;
                    let idx = imm32_operand(operands.get(1))?;
                    let value = read_register(frame, src)?.clone();
                    write_register(frame, idx as u16, value)?;
                    frame.pc += 1;
                }
                Op::TdzError => {
                    return Err(VmError::TemporalDeadZone {
                        local_index: imm32_operand(operands.first())? as u32,
                    });
                }
                Op::NewObject => {
                    let dst = register_operand(operands.first())?;
                    write_register(frame, dst, Value::Object(JsObject::new()))?;
                    frame.pc += 1;
                }
                Op::LoadProperty => {
                    let dst = register_operand(operands.first())?;
                    let obj_reg = register_operand(operands.get(1))?;
                    let name_idx = const_operand(operands.get(2))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let value = match read_register(frame, obj_reg)? {
                        Value::Object(o) => o.get(&name).unwrap_or(Value::Undefined),
                        Value::ClassConstructor(c) => {
                            if name == "prototype" {
                                Value::Object(c.prototype.clone())
                            } else {
                                c.statics.get(&name).unwrap_or(Value::Undefined)
                            }
                        }
                        Value::String(s) if name == "length" => {
                            Value::Number(NumberValue::from_i32(s.len() as i32))
                        }
                        Value::Array(a) if name == "length" => {
                            Value::Number(NumberValue::from_i32(a.len() as i32))
                        }
                        _ => return Err(VmError::TypeMismatch),
                    };
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::StoreProperty => {
                    let obj_reg = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let src = register_operand(operands.get(2))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let value = read_register(frame, src)?.clone();
                    let target = match read_register(frame, obj_reg)? {
                        Value::Object(o) => o.clone(),
                        Value::ClassConstructor(c) => c.statics.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    target.set(&name, value);
                    frame.pc += 1;
                }
                Op::DeleteProperty => {
                    let dst = register_operand(operands.first())?;
                    let obj_reg = register_operand(operands.get(1))?;
                    let name_idx = const_operand(operands.get(2))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let obj = match read_register(frame, obj_reg)? {
                        Value::Object(o) => o.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let removed = obj.delete(&name);
                    write_register(frame, dst, Value::Boolean(removed))?;
                    frame.pc += 1;
                }
                Op::GetPrototype => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let result = match read_register(frame, src)? {
                        Value::Object(o) => match o.prototype() {
                            Some(p) => Value::Object(p),
                            None => Value::Null,
                        },
                        _ => return Err(VmError::TypeMismatch),
                    };
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                Op::SetPrototype => {
                    let obj_reg = register_operand(operands.first())?;
                    let proto_reg = register_operand(operands.get(1))?;
                    let obj = match read_register(frame, obj_reg)? {
                        Value::Object(o) => o.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    // Class values chain through their statics
                    // object — `class D extends C` sets
                    // `D.statics.[[Prototype]] = C.statics` so
                    // `D.staticMethod` walks up to `C.staticMethod`
                    // through the existing prototype lookup.
                    let proto = match read_register(frame, proto_reg)? {
                        Value::Object(p) => Some(p.clone()),
                        Value::ClassConstructor(c) => Some(c.statics.clone()),
                        Value::Null => None,
                        _ => return Err(VmError::TypeMismatch),
                    };
                    obj.set_prototype(proto);
                    frame.pc += 1;
                }
                Op::NewArray => {
                    let dst = register_operand(operands.first())?;
                    let count = match operands.get(1) {
                        Some(&Operand::ConstIndex(n)) => n,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let mut elements: SmallVec<[Value; 4]> =
                        SmallVec::with_capacity(count as usize);
                    for i in 0..count as usize {
                        let r = register_operand(operands.get(2 + i))?;
                        elements.push(read_register(frame, r)?.clone());
                    }
                    write_register(frame, dst, Value::Array(JsArray::from_elements(elements)))?;
                    frame.pc += 1;
                }
                Op::LoadElement => {
                    let dst = register_operand(operands.first())?;
                    let recv_reg = register_operand(operands.get(1))?;
                    let idx_reg = register_operand(operands.get(2))?;
                    let recv = read_register(frame, recv_reg)?.clone();
                    let idx = match read_register(frame, idx_reg)? {
                        Value::Number(n) => match n.as_smi() {
                            Some(v) if v >= 0 => v as usize,
                            _ => return Err(VmError::TypeMismatch),
                        },
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let value = match recv {
                        Value::Array(a) => a.get(idx),
                        Value::String(s) => match s.char_code_at(idx as u32) {
                            Some(unit) => Value::String(crate::JsString::from_utf16_units(
                                &[unit],
                                &self.string_heap,
                            )?),
                            None => Value::String(crate::JsString::empty(&self.string_heap)?),
                        },
                        _ => return Err(VmError::TypeMismatch),
                    };
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::StoreElement => {
                    let arr_reg = register_operand(operands.first())?;
                    let idx_reg = register_operand(operands.get(1))?;
                    let src_reg = register_operand(operands.get(2))?;
                    let arr = match read_register(frame, arr_reg)? {
                        Value::Array(a) => a.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let idx = match read_register(frame, idx_reg)? {
                        Value::Number(n) => match n.as_smi() {
                            Some(v) if v >= 0 => v as usize,
                            _ => return Err(VmError::TypeMismatch),
                        },
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let value = read_register(frame, src_reg)?.clone();
                    arr.set(idx, value);
                    frame.pc += 1;
                }
                Op::ArrayLength => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let arr = match read_register(frame, src)? {
                        Value::Array(a) => a.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let n = NumberValue::from_i32(arr.len() as i32);
                    write_register(frame, dst, Value::Number(n))?;
                    frame.pc += 1;
                }
                Op::Instanceof => {
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let result = match (&lhs, &rhs) {
                        (Value::Object(a), Value::Object(target)) => {
                            // Foundation interpretation: rhs is
                            // the "prototype to look for". Class
                            // lowering (slice 26) replaces this
                            // with a real `rhs.prototype` lookup.
                            a.has_in_proto_chain(target)
                        }
                        _ => false,
                    };
                    write_register(frame, dst, Value::Boolean(result))?;
                    frame.pc += 1;
                }
                Op::Add => {
                    self.run_add(module, &operands, frame)?;
                }
                Op::Sub => {
                    self.run_numeric(&operands, frame, number::sub, bigint_sub_op)?;
                }
                Op::Mul => {
                    self.run_numeric(&operands, frame, number::mul, bigint_mul_op)?;
                }
                Op::Div => {
                    self.run_numeric(&operands, frame, number::div, bigint::ops::div)?;
                }
                Op::Rem => {
                    self.run_numeric(&operands, frame, number::rem, bigint::ops::rem)?;
                }
                Op::Pow => {
                    self.run_numeric(&operands, frame, number::pow, bigint::ops::pow)?;
                }
                Op::BitwiseAnd => {
                    self.run_numeric(&operands, frame, number::bitwise_and, bigint_and_op)?;
                }
                Op::BitwiseOr => {
                    self.run_numeric(&operands, frame, number::bitwise_or, bigint_or_op)?;
                }
                Op::BitwiseXor => {
                    self.run_numeric(&operands, frame, number::bitwise_xor, bigint_xor_op)?;
                }
                Op::Shl => {
                    self.run_numeric(&operands, frame, number::shl, bigint::ops::shl)?;
                }
                Op::Shr => {
                    self.run_numeric(&operands, frame, number::shr_arith, bigint::ops::shr)?;
                }
                Op::Ushr => {
                    // `>>>` on BigInt is a spec TypeError — only the
                    // Number path is allowed here.
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let result = match (&lhs, &rhs) {
                        (Value::Number(a), Value::Number(b)) => {
                            Value::Number(number::shr_logical(*a, *b))
                        }
                        _ => return Err(VmError::TypeMismatch),
                    };
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                Op::Neg => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let value = match read_register(frame, src)? {
                        Value::Number(n) => Value::Number(number::neg(*n)),
                        Value::BigInt(b) => Value::BigInt(bigint::ops::neg(b)),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::BitwiseNot => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    if let Value::BigInt(b) = read_register(frame, src)?.clone() {
                        let value = Value::BigInt(bigint::ops::bitwise_not(&b));
                        write_register(frame, dst, value)?;
                        frame.pc += 1;
                        continue;
                    }
                    let n = read_register(frame, src)?
                        .as_number()
                        .ok_or(VmError::TypeMismatch)?;
                    write_register(frame, dst, Value::Number(number::bitwise_not(n)))?;
                    frame.pc += 1;
                }
                Op::ToNumber => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let value = match read_register(frame, src)? {
                        Value::Number(n) => *n,
                        Value::Boolean(true) => NumberValue::Smi(1),
                        Value::Boolean(false) | Value::Null => NumberValue::Smi(0),
                        // Spec ToNumber(BigInt) is a TypeError; we
                        // surface it here so the unary `+` operator
                        // doesn't silently coerce.
                        Value::BigInt(_) => return Err(VmError::TypeMismatch),
                        Value::Undefined
                        | Value::Function { .. }
                        | Value::Closure { .. }
                        | Value::BoundFunction(_)
                        | Value::Object(_)
                        | Value::Array(_)
                        | Value::Iterator(_)
                        | Value::ClassConstructor(_) => NumberValue::Double(f64::NAN),
                        Value::String(s) => number::to_number_from_string(&s.to_lossy_string()),
                    };
                    write_register(frame, dst, Value::Number(value))?;
                    frame.pc += 1;
                }
                Op::Equal => {
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let eq = lhs == rhs;
                    write_register(frame, dst, Value::Boolean(eq))?;
                    frame.pc += 1;
                }
                Op::NotEqual => {
                    let (dst, lhs, rhs) = self.binop_regs(&operands, frame)?;
                    let eq = lhs == rhs;
                    write_register(frame, dst, Value::Boolean(!eq))?;
                    frame.pc += 1;
                }
                Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
                    self.run_compare(&operands, frame, op)?;
                }
                Op::GetStringIndex => {
                    let dst = register_operand(operands.first())?;
                    let recv = register_operand(operands.get(1))?;
                    let idx_reg = register_operand(operands.get(2))?;
                    let recv_s = read_register(frame, recv)?
                        .as_string()
                        .ok_or(VmError::TypeMismatch)?
                        .clone();
                    let idx = match read_register(frame, idx_reg)? {
                        Value::Number(n) => match n.as_smi() {
                            Some(v) if v >= 0 => v as u32,
                            _ => recv_s.len(), // out of range → empty
                        },
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let result_str = match recv_s.char_code_at(idx) {
                        Some(unit) => JsString::from_utf16_units(&[unit], &self.string_heap)?,
                        None => JsString::empty(&self.string_heap)?,
                    };
                    write_register(frame, dst, Value::String(result_str))?;
                    frame.pc += 1;
                }
                Op::LoadThis => {
                    let dst = register_operand(operands.first())?;
                    let value = frame.this_value.clone();
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::NewError => {
                    let dst = register_operand(operands.first())?;
                    let msg_reg = register_operand(operands.get(1))?;
                    let value = read_register(frame, msg_reg)?.clone();
                    let message_str = match value {
                        Value::Undefined => None,
                        Value::String(s) => Some(s),
                        other => {
                            let s = JsString::from_str(&other.display_string(), &self.string_heap)?;
                            Some(s)
                        }
                    };
                    let obj = JsObject::new();
                    let name = JsString::from_str("Error", &self.string_heap)?;
                    obj.set("name", Value::String(name));
                    if let Some(s) = message_str {
                        obj.set("message", Value::String(s));
                    }
                    write_register(frame, dst, Value::Object(obj))?;
                    frame.pc += 1;
                }
                Op::MathLoad => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let name = lookup_string_constant(module, name_idx)?;
                    let value =
                        math::load_constant(&name).ok_or_else(|| VmError::UnknownIntrinsic {
                            name: format!("Math.{name}"),
                        })?;
                    write_register(frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::MathCall => {
                    let dst = register_operand(operands.first())?;
                    let name_idx = const_operand(operands.get(1))?;
                    let argc = match operands.get(2) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = lookup_string_constant(module, name_idx)?;
                    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(3 + i))?;
                        args.push(read_register(frame, r)?.clone());
                    }
                    let result = math::call(&name, &args).map_err(math_to_vm_error)?;
                    write_register(frame, dst, result)?;
                    frame.pc += 1;
                }
                Op::CollectRest => {
                    let dst = register_operand(operands.first())?;
                    // Drain rather than clone — the rest array is
                    // built once per call and CollectRest is the
                    // single consumer, so freeing the backing
                    // storage promptly keeps frame sizes small.
                    let elements: SmallVec<[Value; 4]> = std::mem::take(&mut frame.rest_args);
                    write_register(frame, dst, Value::Array(JsArray::from_elements(elements)))?;
                    frame.pc += 1;
                }
                Op::MakeClass => {
                    let dst = register_operand(operands.first())?;
                    let ctor_reg = register_operand(operands.get(1))?;
                    let proto_reg = register_operand(operands.get(2))?;
                    let statics_reg = register_operand(operands.get(3))?;
                    let ctor = read_register(frame, ctor_reg)?.clone();
                    if !is_callable(&ctor) {
                        return Err(VmError::NotCallable);
                    }
                    let prototype = match read_register(frame, proto_reg)? {
                        Value::Object(o) => o.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let statics = match read_register(frame, statics_reg)? {
                        Value::Object(o) => o.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let class = std::rc::Rc::new(ClassConstructor {
                        ctor,
                        prototype,
                        statics,
                    });
                    write_register(frame, dst, Value::ClassConstructor(class))?;
                    frame.pc += 1;
                }
                Op::EnterTry => {
                    let catch_off = imm32_operand(operands.first())?;
                    let finally_off = imm32_operand(operands.get(1))?;
                    let exc_register = register_operand(operands.get(2))?;
                    let next_pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)? as i64;
                    let resolve = |off: i32| -> Result<Option<u32>, VmError> {
                        if off == NO_HANDLER_OFFSET {
                            return Ok(None);
                        }
                        let target = next_pc + off as i64;
                        if target < 0 || target > u32::MAX as i64 {
                            return Err(VmError::InvalidOperand);
                        }
                        Ok(Some(target as u32))
                    };
                    let catch_pc = resolve(catch_off)?;
                    let finally_pc = resolve(finally_off)?;
                    if catch_pc.is_none() && finally_pc.is_none() {
                        return Err(VmError::InvalidOperand);
                    }
                    frame.handlers.push(TryHandler {
                        catch_pc,
                        finally_pc,
                        exc_register,
                    });
                    frame.pc += 1;
                }
                Op::LeaveTry => {
                    if frame.handlers.pop().is_none() {
                        return Err(VmError::InvalidOperand);
                    }
                    frame.pc += 1;
                }
                Op::BindFunction => {
                    let dst = register_operand(operands.first())?;
                    let callee_reg = register_operand(operands.get(1))?;
                    let this_reg = register_operand(operands.get(2))?;
                    let argc = match operands.get(3) {
                        Some(&Operand::ConstIndex(n)) => n as usize,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let target = read_register(frame, callee_reg)?.clone();
                    if !is_callable(&target) {
                        return Err(VmError::NotCallable);
                    }
                    let bound_this = read_register(frame, this_reg)?.clone();
                    let mut bound_args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
                    for i in 0..argc {
                        let r = register_operand(operands.get(4 + i))?;
                        bound_args.push(read_register(frame, r)?.clone());
                    }
                    let bound = std::rc::Rc::new(BoundFunction {
                        target,
                        bound_this,
                        bound_args,
                    });
                    write_register(frame, dst, Value::BoundFunction(bound))?;
                    frame.pc += 1;
                }
                Op::GetIterator => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let value = read_register(frame, src)?.clone();
                    let state = match value {
                        Value::Array(array) => IteratorState::Array { array, index: 0 },
                        Value::String(string) => IteratorState::String { string, index: 0 },
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let iter = std::rc::Rc::new(std::cell::RefCell::new(state));
                    write_register(frame, dst, Value::Iterator(iter))?;
                    frame.pc += 1;
                }
                Op::IteratorNext => {
                    let value_dst = register_operand(operands.first())?;
                    let done_dst = register_operand(operands.get(1))?;
                    let iter_reg = register_operand(operands.get(2))?;
                    let iter = match read_register(frame, iter_reg)? {
                        Value::Iterator(rc) => rc.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let (value, done) = step_iterator(&iter, &self.string_heap)?;
                    write_register(frame, value_dst, value)?;
                    write_register(frame, done_dst, Value::Boolean(done))?;
                    frame.pc += 1;
                }
                Op::ArrayPush => {
                    let arr_reg = register_operand(operands.first())?;
                    let value_reg = register_operand(operands.get(1))?;
                    let value = read_register(frame, value_reg)?.clone();
                    let array = match read_register(frame, arr_reg)? {
                        Value::Array(a) => a.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    let next_idx = array.len();
                    array.set(next_idx, value);
                    frame.pc += 1;
                }
            }
        }
    }
}

impl Interpreter {
    /// Pop the top frame and write its result into the caller's
    /// `return_register`. Returns `Some(value)` when the script
    /// completes (`<main>` popped) so `run` can return that value
    /// as the program's completion.
    fn pop_frame(
        &self,
        stack: &mut SmallVec<[Frame; 8]>,
        value: Value,
    ) -> Result<Option<Value>, VmError> {
        let popped = stack.pop().ok_or(VmError::InvalidOperand)?;
        // For frames entered via `Op::New`, ES spec
        // `OrdinaryConstruct` step 11: if the constructor returned
        // an object, hand that back; otherwise the freshly
        // allocated `this` is the result.
        let resolved = match popped.construct_target {
            Some(target) => match value {
                Value::Object(_) | Value::Array(_) => value,
                _ => Value::Object(target),
            },
            None => value,
        };
        let Some(return_reg) = popped.return_register else {
            return Ok(Some(resolved));
        };
        let caller = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(caller, return_reg, resolved)?;
        // Caller's pc was set to the next instruction at call time;
        // nothing to advance here.
        Ok(None)
    }

    /// Handle `Op::Call`: push a new frame for the callee with
    /// arguments copied into the parameter slots and `this` bound
    /// to `Value::Undefined` (foundation strict default).
    fn do_call(
        &self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };

        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc as usize);
        for i in 0..argc as usize {
            let r = register_operand(operands.get(3 + i))?;
            args.push(read_register(&stack[top_idx], r)?.clone());
        }
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, module, &callee, Value::Undefined, args, dst)
    }

    /// Invoke `callee` with the explicit receiver `this_value` and
    /// the given argument list. Centralizes the BoundFunction
    /// unwrapping, closure `bound_this` override, and frame push so
    /// every call opcode (`Op::Call`, `Op::CallWithThis`,
    /// `Op::CallMethodValue`) shares one path.
    ///
    /// `dst` is the **caller's** register that should receive the
    /// completion value when the callee returns. `caller_pc` must
    /// already be advanced before this call so the post-pop
    /// dispatch resumes after the originating instruction.
    fn invoke(
        &self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        // Walk through any number of `bind` layers, accumulating
        // their bound arguments and overriding `this_value` with
        // the innermost `bound_this`. The loop bound matches the
        // JS-call stack-depth limit so a pathological self-bound
        // chain still surfaces as `StackOverflow` rather than
        // unbounded recursion.
        let mut current = callee.clone();
        let mut effective_this = this_value;
        let mut effective_args = args;
        let mut hops: u32 = 0;
        loop {
            if hops >= self.max_stack_depth {
                return Err(VmError::StackOverflow {
                    limit: self.max_stack_depth,
                });
            }
            match current {
                Value::BoundFunction(bound) => {
                    hops += 1;
                    let mut combined: SmallVec<[Value; 8]> =
                        SmallVec::with_capacity(bound.bound_args.len() + effective_args.len());
                    combined.extend(bound.bound_args.iter().cloned());
                    combined.extend(effective_args);
                    effective_this = bound.bound_this.clone();
                    effective_args = combined;
                    current = bound.target.clone();
                }
                Value::ClassConstructor(cc) => {
                    hops += 1;
                    current = cc.ctor.clone();
                }
                _ => break,
            }
        }
        let (function_id, parent_upvalues, this_for_callee) = match current {
            Value::Function { function_id } => {
                (function_id, std::rc::Rc::from(Vec::new()), effective_this)
            }
            Value::Closure {
                function_id,
                upvalues,
                bound_this,
            } => {
                let this_value = match bound_this {
                    Some(t) => *t,
                    None => effective_this,
                };
                (function_id, upvalues, this_value)
            }
            _ => return Err(VmError::NotCallable),
        };

        if stack.len() as u32 >= self.max_stack_depth {
            return Err(VmError::StackOverflow {
                limit: self.max_stack_depth,
            });
        }
        let function = module
            .functions
            .get(function_id as usize)
            .ok_or(VmError::InvalidOperand)?;
        let mut new_frame = Frame::with_return_upvalues_and_this(
            function,
            Some(dst),
            parent_upvalues,
            this_for_callee,
        );
        // Bind parameters: extra args are dropped, missing args
        // stay `Value::Undefined` (matches JS semantics).
        let bind_count = (function.param_count as usize).min(effective_args.len());
        let total_args = effective_args.len();
        let mut iter = effective_args.into_iter();
        for i in 0..bind_count {
            let value = iter.next().expect("bind_count <= len");
            let slot = new_frame
                .registers
                .get_mut(i)
                .ok_or(VmError::InvalidOperand)?;
            *slot = value;
        }
        // Stash the trailing args for `Op::CollectRest`. Only the
        // rest-aware callees pay the allocation; everyone else
        // leaves `rest_args` empty as initialised.
        if function.has_rest && total_args > function.param_count as usize {
            new_frame.rest_args = iter.collect();
        }
        stack.push(new_frame);
        Ok(())
    }

    /// Walk the live frame stack looking for a try-handler that
    /// can absorb an in-flight throw. Within each frame, the
    /// handler stack is searched innermost-first:
    ///
    /// - **Catch handler hit** — the thrown value is written into
    ///   the handler's `exc_register`, the frame's pc jumps to the
    ///   catch entry, the matching handler is popped, and dispatch
    ///   resumes in that frame.
    /// - **Finally-only handler hit** — the thrown value is parked
    ///   on `frame.pending_throw`, the pc jumps to the finally
    ///   entry, and the matching handler is popped.
    ///   [`Op::EndFinally`] consumes the parked value to re-throw.
    /// - **No handler in this frame** — the frame is popped and the
    ///   search continues in the caller.
    ///
    /// Returns `Err(VmError::Uncaught { value })` when the frame
    /// stack empties without a handler. The caller is expected to
    /// propagate this as the script's failure outcome.
    fn unwind_throw(stack: &mut SmallVec<[Frame; 8]>, value: Value) -> Result<(), VmError> {
        let display = value.display_string();
        let payload = value;
        loop {
            let Some(frame) = stack.last_mut() else {
                return Err(VmError::Uncaught { value: display });
            };
            let Some(handler) = frame.handlers.pop() else {
                stack.pop();
                continue;
            };
            if let Some(catch_pc) = handler.catch_pc {
                frame.pc = catch_pc;
                let slot = frame
                    .registers
                    .get_mut(handler.exc_register as usize)
                    .ok_or(VmError::InvalidOperand)?;
                *slot = payload;
                return Ok(());
            }
            let finally_pc = handler.finally_pc.ok_or(VmError::InvalidOperand)?;
            frame.pc = finally_pc;
            frame.pending_throw = Some(payload);
            return Ok(());
        }
    }

    /// Handle `Op::New`: allocate a fresh receiver, set its
    /// `[[Prototype]]` to `callee.prototype` (when present), and
    /// invoke the callee with `this = receiver`. The caller's `dst`
    /// register receives either the constructor's returned object
    /// or the freshly allocated receiver — `pop_frame` performs
    /// that swap so the unwind path is uniform across call shapes.
    fn do_construct(
        &self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let argc = match operands.get(2) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        if !is_callable(&callee) {
            return Err(VmError::NotCallable);
        }
        // Allocate receiver and link its prototype before pushing
        // the new frame. The constructor might mutate the receiver
        // immediately, so the prototype link must already be in
        // place.
        let receiver = JsObject::new();
        if let Some(proto) = construct_prototype(&callee) {
            receiver.set_prototype(Some(proto));
        }
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc as usize);
        for i in 0..argc as usize {
            let r = register_operand(operands.get(3 + i))?;
            args.push(read_register(&stack[top_idx], r)?.clone());
        }
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        let this_value = Value::Object(receiver.clone());
        self.invoke(stack, module, &callee, this_value, args, dst)?;
        // The pushed frame is now on top; mark it so `pop_frame`
        // can substitute the receiver for any non-object return.
        if let Some(top) = stack.last_mut() {
            top.construct_target = Some(receiver);
        }
        Ok(())
    }

    /// Handle `Op::CallSpread`: read the args array, fan it out
    /// into the standard call path. The receiver register holds
    /// the explicit `this` value (foundation lowers free spread
    /// calls with `this = undefined`).
    fn do_call_spread(
        &self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let args_reg = register_operand(operands.get(3))?;
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        let this_value = read_register(&stack[top_idx], this_reg)?.clone();
        let args_array = match read_register(&stack[top_idx], args_reg)? {
            Value::Array(a) => a.clone(),
            _ => return Err(VmError::TypeMismatch),
        };
        let args: SmallVec<[Value; 8]> = args_array.borrow_body().iter().cloned().collect();
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, module, &callee, this_value, args, dst)
    }

    /// Handle `Op::CallWithThis`: same as `do_call` but the call
    /// site supplies an explicit `this` register. Used by
    /// `Function.prototype.call` lowering and the array-literal
    /// path of `Function.prototype.apply`.
    fn do_call_with_this(
        &self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(&Operand::ConstIndex(n)) => n,
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;
        let callee = read_register(&stack[top_idx], callee_reg)?.clone();
        let this_value = read_register(&stack[top_idx], this_reg)?.clone();
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc as usize);
        for i in 0..argc as usize {
            let r = register_operand(operands.get(4 + i))?;
            args.push(read_register(&stack[top_idx], r)?.clone());
        }
        stack[top_idx].pc = stack[top_idx]
            .pc
            .checked_add(1)
            .ok_or(VmError::InvalidOperand)?;
        self.invoke(stack, module, &callee, this_value, args, dst)
    }

    /// Handle `Op::CallMethodValue`: the universal method-call op.
    /// Branches by receiver kind:
    /// - `String` / `Array` — synchronous intrinsic-table dispatch.
    ///   Result lands in the destination register without pushing
    ///   a frame.
    /// - `Object` — load the property; raise `NotCallable` if the
    ///   resolved value is not a function; otherwise call it with
    ///   `this = receiver`.
    /// - `Function` / `Closure` / `BoundFunction` — only the
    ///   `call`, `apply`, and `bind` shapes are recognised; anything
    ///   else surfaces as `UnknownIntrinsic`.
    fn do_call_method_value(
        &self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let recv_reg = register_operand(operands.get(1))?;
        let name_idx = const_operand(operands.get(2))?;
        let argc = match operands.get(3) {
            Some(&Operand::ConstIndex(n)) => n as usize,
            _ => return Err(VmError::InvalidOperand),
        };
        let name = match module.constants.get(name_idx as usize) {
            Some(Constant::String { utf16 }) => String::from_utf16_lossy(utf16),
            _ => return Err(VmError::InvalidOperand),
        };
        let top_idx = stack.len() - 1;
        let recv_value = read_register(&stack[top_idx], recv_reg)?.clone();
        let mut arg_values: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc);
        for i in 0..argc {
            let r = register_operand(operands.get(4 + i))?;
            arg_values.push(read_register(&stack[top_idx], r)?.clone());
        }

        // Primitive prototypes go through the intrinsic table —
        // synchronous, no frame push, advance pc and write directly.
        let intrinsic = match &recv_value {
            Value::String(_) => string_prototype::lookup(&name),
            Value::Array(_) => array_prototype::lookup(&name),
            Value::Number(_) => number::prototype_lookup(&name),
            _ => None,
        };
        if let Some(entry) = intrinsic {
            let small_args: SmallVec<[Value; 4]> = arg_values.iter().cloned().collect();
            let result = (entry.impl_fn)(&IntrinsicArgs {
                receiver: &recv_value,
                args: &small_args,
                string_heap: &self.string_heap,
            })
            .map_err(intrinsic_to_vm_error)?;
            let frame = &mut stack[top_idx];
            write_register(frame, dst, result)?;
            frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
            return Ok(());
        }

        // Property-bearing receivers — load the property first.
        // For class constructors, `prototype` resolves to the
        // instance prototype object (mirroring `Op::LoadProperty`'s
        // class shape) and other names walk the static side. Only
        // when the property lookup hands back a callable do we
        // dispatch with `this = recv`; missing or non-callable
        // properties surface as `NotCallable` so callers see the
        // same error as `obj.notFn()`.
        let lookup_via_property = match &recv_value {
            Value::Object(obj) => Some(obj.get(&name).unwrap_or(Value::Undefined)),
            Value::ClassConstructor(c) => Some(if name == "prototype" {
                Value::Object(c.prototype.clone())
            } else {
                c.statics.get(&name).unwrap_or(Value::Undefined)
            }),
            _ => None,
        };
        if let Some(method) = lookup_via_property {
            if !is_callable(&method) {
                return Err(VmError::NotCallable);
            }
            stack[top_idx].pc = stack[top_idx]
                .pc
                .checked_add(1)
                .ok_or(VmError::InvalidOperand)?;
            return self.invoke(stack, module, &method, recv_value.clone(), arg_values, dst);
        }

        // `Function.prototype.{call, apply, bind}` on a callable
        // receiver that doesn't expose the method as a property.
        // `apply` only accepts an `Array` (or omitted / null /
        // undefined) for its second argument.
        if is_callable(&recv_value) {
            return self.dispatch_function_method(
                stack,
                module,
                &recv_value,
                &name,
                arg_values,
                dst,
            );
        }

        Err(VmError::UnknownIntrinsic { name })
    }

    /// Dispatch `call` / `apply` / `bind` on a callable receiver.
    /// Foundation handles only the literal-array shape of `apply`
    /// — non-array second arguments raise `TypeMismatch` so callers
    /// learn quickly that the foundation slice rejects dynamic
    /// argument arrays.
    fn dispatch_function_method(
        &self,
        stack: &mut SmallVec<[Frame; 8]>,
        module: &BytecodeModule,
        callee: &Value,
        name: &str,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        let top_idx = stack.len() - 1;
        match name {
            "call" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::Undefined);
                let forwarded: SmallVec<[Value; 8]> = iter.collect();
                stack[top_idx].pc = stack[top_idx]
                    .pc
                    .checked_add(1)
                    .ok_or(VmError::InvalidOperand)?;
                self.invoke(stack, module, callee, this_value, forwarded, dst)
            }
            "apply" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::Undefined);
                let forwarded: SmallVec<[Value; 8]> = match iter.next() {
                    None | Some(Value::Undefined) | Some(Value::Null) => SmallVec::new(),
                    Some(Value::Array(arr)) => arr.borrow_body().iter().cloned().collect(),
                    _ => return Err(VmError::TypeMismatch),
                };
                stack[top_idx].pc = stack[top_idx]
                    .pc
                    .checked_add(1)
                    .ok_or(VmError::InvalidOperand)?;
                self.invoke(stack, module, callee, this_value, forwarded, dst)
            }
            "bind" => {
                let mut iter = args.into_iter();
                let this_value = iter.next().unwrap_or(Value::Undefined);
                let bound_args: SmallVec<[Value; 4]> = iter.collect();
                let bound = std::rc::Rc::new(BoundFunction {
                    target: callee.clone(),
                    bound_this: this_value,
                    bound_args,
                });
                let frame = &mut stack[top_idx];
                write_register(frame, dst, Value::BoundFunction(bound))?;
                frame.pc = frame.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
                Ok(())
            }
            _ => Err(VmError::UnknownIntrinsic {
                name: name.to_string(),
            }),
        }
    }

    fn binop_regs(
        &self,
        operands: &[Operand],
        frame: &Frame,
    ) -> Result<(u16, Value, Value), VmError> {
        let dst = register_operand(operands.first())?;
        let lhs = register_operand(operands.get(1))?;
        let rhs = register_operand(operands.get(2))?;
        let l = read_register(frame, lhs)?.clone();
        let r = read_register(frame, rhs)?.clone();
        Ok((dst, l, r))
    }

    fn run_numeric(
        &self,
        operands: &[Operand],
        frame: &mut Frame,
        op: fn(NumberValue, NumberValue) -> NumberValue,
        bigint_op: BigIntBinop,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = self.binop_regs(operands, frame)?;
        let result = match (&lhs, &rhs) {
            (Value::Number(a), Value::Number(b)) => Value::Number(op(*a, *b)),
            (Value::BigInt(a), Value::BigInt(b)) => {
                Value::BigInt(bigint_op(a, b).map_err(bigint_to_vm_error)?)
            }
            // Mixed Number/BigInt is a spec TypeError.
            (Value::Number(_), Value::BigInt(_)) | (Value::BigInt(_), Value::Number(_)) => {
                return Err(VmError::TypeMismatch);
            }
            _ => return Err(VmError::TypeMismatch),
        };
        write_register(frame, dst, result)?;
        frame.pc += 1;
        Ok(())
    }

    fn run_add(
        &self,
        _module: &BytecodeModule,
        operands: &[Operand],
        frame: &mut Frame,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = self.binop_regs(operands, frame)?;
        let result = match (&lhs, &rhs) {
            (Value::Number(a), Value::Number(b)) => Value::Number(number::add(*a, *b)),
            (Value::BigInt(a), Value::BigInt(b)) => Value::BigInt(bigint::ops::add(a, b)),
            (Value::Number(_), Value::BigInt(_)) | (Value::BigInt(_), Value::Number(_)) => {
                return Err(VmError::TypeMismatch);
            }
            (Value::String(a), Value::String(b)) => {
                Value::String(JsString::concat(a, b, &self.string_heap)?)
            }
            _ => return Err(VmError::TypeMismatch),
        };
        write_register(frame, dst, result)?;
        frame.pc += 1;
        Ok(())
    }

    fn run_compare(&self, operands: &[Operand], frame: &mut Frame, op: Op) -> Result<(), VmError> {
        let (dst, lhs, rhs) = self.binop_regs(operands, frame)?;
        let truthy = match (&lhs, &rhs) {
            (Value::Number(a), Value::Number(b)) => {
                ordering_matches_op(op, number_ordering_to_std(number::compare(*a, *b)))
            }
            (Value::BigInt(a), Value::BigInt(b)) => {
                ordering_matches_op(op, Some(bigint::ops::compare(a, b)))
            }
            (Value::BigInt(a), Value::Number(b)) => {
                ordering_matches_op(op, bigint::ops::compare_to_f64(a, b.as_f64()))
            }
            (Value::Number(a), Value::BigInt(b)) => ordering_matches_op(
                op,
                bigint::ops::compare_to_f64(b, a.as_f64()).map(std::cmp::Ordering::reverse),
            ),
            (Value::String(a), Value::String(b)) => {
                let ord = a.compare_lex(b);
                match op {
                    Op::LessThan => ord.is_lt(),
                    Op::LessEq => ord.is_le(),
                    Op::GreaterThan => ord.is_gt(),
                    Op::GreaterEq => ord.is_ge(),
                    _ => unreachable!(),
                }
            }
            _ => return Err(VmError::TypeMismatch),
        };
        write_register(frame, dst, Value::Boolean(truthy))?;
        frame.pc += 1;
        Ok(())
    }
}

/// Function-pointer alias for the BigInt sibling of the
/// `NumberValue` arithmetic helpers. A few `BigInt` ops can fail
/// (division by zero, negative exponent, oversized shift); the
/// VM dispatcher maps each error variant to the matching
/// `VmError`.
type BigIntBinop = fn(
    &bigint::BigIntValue,
    &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError>;

fn bigint_sub_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::sub(a, b))
}

fn bigint_mul_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::mul(a, b))
}

fn bigint_and_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_and(a, b))
}

fn bigint_or_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_or(a, b))
}

fn bigint_xor_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_xor(a, b))
}

/// Map [`bigint::ops::OpError`] into the surrounding [`VmError`].
fn bigint_to_vm_error(err: bigint::ops::OpError) -> VmError {
    match err {
        bigint::ops::OpError::DivisionByZero
        | bigint::ops::OpError::NegativeExponent
        | bigint::ops::OpError::ShiftOutOfRange => VmError::TypeMismatch,
    }
}

/// Convert [`number::NumericOrdering`] (which carries an extra
/// `Unordered` variant for `NaN`) into the standard library's
/// `Ordering` paired with an `Option`. `None` means "NaN seen,
/// any relational result is `false`".
fn number_ordering_to_std(o: NumericOrdering) -> Option<std::cmp::Ordering> {
    match o {
        NumericOrdering::Less => Some(std::cmp::Ordering::Less),
        NumericOrdering::Equal => Some(std::cmp::Ordering::Equal),
        NumericOrdering::Greater => Some(std::cmp::Ordering::Greater),
        NumericOrdering::Unordered => None,
    }
}

/// Apply a `<`, `<=`, `>`, or `>=` opcode to an `Ordering`.
/// `None` (one operand was `NaN` or otherwise unordered) yields
/// `false` for every relational op per spec.
fn ordering_matches_op(op: Op, ord: Option<std::cmp::Ordering>) -> bool {
    let Some(o) = ord else {
        return false;
    };
    match op {
        Op::LessThan => o.is_lt(),
        Op::LessEq => o.is_le() || o.is_eq(),
        Op::GreaterThan => o.is_gt(),
        Op::GreaterEq => o.is_ge() || o.is_eq(),
        _ => unreachable!(),
    }
}

/// Walk a live frame stack top-down and build a snapshot the
/// runtime / CLI can render. Top-of-stack first.
fn snapshot_frames(module: &BytecodeModule, stack: &[Frame]) -> Vec<StackFrameSnapshot> {
    stack
        .iter()
        .rev()
        .map(|f| {
            let function = module.functions.get(f.function_id as usize);
            let function_name = function
                .map(|fun| fun.name.clone())
                .unwrap_or_else(|| "<unknown>".to_string());
            let span = function
                .and_then(|fun| fun.spans.iter().find(|s| s.pc == f.pc).map(|s| s.span))
                .or_else(|| function.map(|fun| fun.span))
                .unwrap_or((0, 0));
            StackFrameSnapshot {
                function_name,
                module: module.module.clone(),
                span,
            }
        })
        .collect()
}

fn math_to_vm_error(err: math::MathError) -> VmError {
    match err {
        math::MathError::UnknownMember(name) => VmError::UnknownIntrinsic {
            name: format!("Math.{name}"),
        },
        math::MathError::BadArgument { .. } => VmError::TypeMismatch,
    }
}

fn intrinsic_to_vm_error(err: IntrinsicError) -> VmError {
    match err {
        IntrinsicError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        IntrinsicError::BadReceiver { .. } | IntrinsicError::BadArgument { .. } => {
            VmError::TypeMismatch
        }
        IntrinsicError::UnknownMethod { name } => VmError::UnknownIntrinsic { name },
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

fn register_operand(operand: Option<&Operand>) -> Result<u16, VmError> {
    match operand {
        Some(Operand::Register(r)) => Ok(*r),
        _ => Err(VmError::InvalidOperand),
    }
}

fn const_operand(operand: Option<&Operand>) -> Result<u32, VmError> {
    match operand {
        Some(Operand::ConstIndex(k)) => Ok(*k),
        _ => Err(VmError::InvalidOperand),
    }
}

/// Resolve a string constant referenced by index. Returned as a
/// Rust `String` because `JsObject` keys are stored UTF-8 in this
/// slice; task 18 (shapes) revisits the key representation.
fn lookup_string_constant(module: &BytecodeModule, idx: u32) -> Result<String, VmError> {
    match module.constants.get(idx as usize) {
        Some(Constant::String { utf16 }) => Ok(String::from_utf16_lossy(utf16)),
        _ => Err(VmError::InvalidOperand),
    }
}

fn imm32_operand(operand: Option<&Operand>) -> Result<i32, VmError> {
    match operand {
        Some(Operand::Imm32(v)) => Ok(*v),
        _ => Err(VmError::InvalidOperand),
    }
}

/// Apply a relative branch. Negative offsets are back-edges and
/// poll the interrupt flag — that's the foundation plan's
/// `every back-edge polls the runtime checkpoint` rule.
fn apply_branch(frame: &mut Frame, offset: i32, interrupt: &InterruptFlag) -> Result<(), VmError> {
    let next_pc = (frame.pc as i64 + 1).saturating_add(offset as i64);
    if next_pc < 0 || next_pc > u32::MAX as i64 {
        return Err(VmError::InvalidOperand);
    }
    if offset < 0 && interrupt.is_set() {
        return Err(VmError::Interrupted);
    }
    frame.pc = next_pc as u32;
    Ok(())
}

fn read_register(frame: &Frame, idx: u16) -> Result<&Value, VmError> {
    frame
        .registers
        .get(idx as usize)
        .ok_or(VmError::InvalidOperand)
}

fn write_register(frame: &mut Frame, idx: u16, value: Value) -> Result<(), VmError> {
    let slot = frame
        .registers
        .get_mut(idx as usize)
        .ok_or(VmError::InvalidOperand)?;
    *slot = value;
    Ok(())
}

/// Drive an iterator one step. Returns `(value, done)`. Once an
/// iterator hands back `done = true`, its state transitions to
/// `Exhausted` so subsequent calls are stable no-ops (matches the
/// spec rule "an iterator never produces values after it has
/// produced `done: true`"; §7.4.2 step 6).
fn step_iterator(
    iter: &std::rc::Rc<std::cell::RefCell<IteratorState>>,
    string_heap: &StringHeap,
) -> Result<(Value, bool), VmError> {
    let mut state = iter.borrow_mut();
    let outcome = match &mut *state {
        IteratorState::Array { array, index } => {
            if *index >= array.len() {
                None
            } else {
                let v = array.get(*index);
                *index += 1;
                Some(v)
            }
        }
        IteratorState::String { string, index } => {
            if let Some(unit) = string.char_code_at(*index) {
                let s = JsString::from_utf16_units(&[unit], string_heap)?;
                *index += 1;
                Some(Value::String(s))
            } else {
                None
            }
        }
        IteratorState::Exhausted => None,
    };
    match outcome {
        Some(value) => Ok((value, false)),
        None => {
            *state = IteratorState::Exhausted;
            Ok((Value::Undefined, true))
        }
    }
}

/// Look up the `prototype` own property of a callable so the
/// `Op::New` path can link the freshly allocated receiver. The
/// foundation supports only object-shaped prototypes: anything
/// else (or a missing `prototype`) leaves the receiver's chain
/// unset, matching `Object.create(null)` semantics. For
/// `Value::Function` (no own properties yet) we always fall back
/// to no prototype; closures created by the class lowering carry
/// `prototype` on the constructor's *own* property table by way
/// of the `BoundFunction` / object surface — but native bytecode
/// `Value::Function` values have no per-id property store, so
/// proto-linking only fires for closures whose function table id
/// has been augmented at class-build time. For the foundation
/// slice that distinction is invisible because the compiler always
/// installs `prototype` through a separate `StoreProperty` on a
/// constructor object reference (the constructor itself is held in
/// a register, with `prototype` set via `obj.prototype = …` style
/// dispatch only on the rare path).
fn construct_prototype(callee: &Value) -> Option<JsObject> {
    match callee {
        Value::ClassConstructor(c) => Some(c.prototype.clone()),
        Value::Object(obj) => match obj.get("prototype") {
            Some(Value::Object(p)) => Some(p),
            _ => None,
        },
        Value::BoundFunction(b) => construct_prototype(&b.target),
        _ => None,
    }
}

/// `true` when `value` is one of the call-site shapes the dispatcher
/// can invoke: a bytecode function, a closure, or a bound function.
/// `Value::BoundFunction` is treated as callable even when it wraps
/// another bound function — the call dispatcher unwraps the chain.
fn is_callable(value: &Value) -> bool {
    matches!(
        value,
        Value::Function { .. }
            | Value::Closure { .. }
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{
        Function, Instruction, Op, Operand, SourceKind as BcSourceKind, SpanEntry,
    };

    fn module_with(code: Vec<Instruction>, scratch: u16) -> BytecodeModule {
        let spans: Vec<SpanEntry> = code
            .iter()
            .map(|i| SpanEntry {
                pc: i.pc,
                span: (0, 0),
            })
            .collect();
        BytecodeModule {
            module: "test.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch,
                param_count: 0,
                own_upvalue_count: 0,
                is_arrow: false,
                has_rest: false,
                code,
                spans,
            }],
            constants: vec![],
        }
    }

    #[test]
    fn returns_undefined_for_load_then_return() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadUndefined,
                    operands: vec![Operand::Register(0)],
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)],
                },
            ],
            1,
        );
        let interp = Interpreter::new();
        assert_eq!(interp.run(&module).unwrap(), Value::Undefined);
    }

    #[test]
    fn missing_return_errors() {
        let module = module_with(
            vec![Instruction {
                pc: 0,
                op: Op::Nop,
                operands: vec![],
            }],
            0,
        );
        let interp = Interpreter::new();
        assert_eq!(
            interp.run(&module).unwrap_err().error,
            VmError::MissingReturn
        );
    }

    #[test]
    fn unwind_throw_pops_frames_until_handler_or_uncaught() {
        // No handlers anywhere in the stack: the throw escapes as
        // VmError::Uncaught carrying the rendered value.
        let main = Function {
            id: 0,
            name: "<main>".to_string(),
            span: (0, 0),
            locals: 0,
            scratch: 1,
            param_count: 0,
            own_upvalue_count: 0,
            is_arrow: false,
            has_rest: false,
            code: vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![],
            }],
            spans: vec![SpanEntry {
                pc: 0,
                span: (0, 0),
            }],
        };
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&main));
        // Push a second frame on top — should be popped during
        // unwinding and not absorb the throw.
        stack.push(Frame::for_function(&main));
        let err = Interpreter::unwind_throw(&mut stack, Value::Boolean(true)).unwrap_err();
        match err {
            VmError::Uncaught { value } => assert_eq!(value, "true"),
            other => panic!("expected Uncaught, got {other:?}"),
        }
        assert!(stack.is_empty(), "frames should be drained on uncaught");
    }

    #[test]
    fn unwind_throw_lands_in_catch_handler() {
        let main = Function {
            id: 0,
            name: "<main>".to_string(),
            span: (0, 0),
            locals: 0,
            scratch: 2,
            param_count: 0,
            own_upvalue_count: 0,
            is_arrow: false,
            has_rest: false,
            code: vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![],
            }],
            spans: vec![SpanEntry {
                pc: 0,
                span: (0, 0),
            }],
        };
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        let mut frame = Frame::for_function(&main);
        frame.handlers.push(TryHandler {
            catch_pc: Some(42),
            finally_pc: None,
            exc_register: 1,
        });
        stack.push(frame);
        Interpreter::unwind_throw(&mut stack, Value::Boolean(true)).unwrap();
        assert_eq!(stack[0].pc, 42);
        assert_eq!(stack[0].registers[1], Value::Boolean(true));
        assert!(stack[0].handlers.is_empty());
    }

    #[test]
    fn is_callable_recognises_call_shapes() {
        assert!(is_callable(&Value::Function { function_id: 7 }));
        assert!(is_callable(&Value::Closure {
            function_id: 7,
            upvalues: std::rc::Rc::from(Vec::new()),
            bound_this: None,
        }));
        let bound = std::rc::Rc::new(BoundFunction {
            target: Value::Function { function_id: 7 },
            bound_this: Value::Undefined,
            bound_args: SmallVec::new(),
        });
        assert!(is_callable(&Value::BoundFunction(bound)));
        assert!(!is_callable(&Value::Number(NumberValue::Smi(1))));
        assert!(!is_callable(&Value::Object(JsObject::new())));
    }

    #[test]
    fn arrow_closure_overrides_call_site_this() {
        // <main>: r0 = LoadThis; Return r0
        // The arrow closure wraps function id 1 with `is_arrow=true`
        // and a `bound_this = Some({tag: "outer"})`. We sneak the
        // bound `this` in by hand-building the closure value rather
        // than going through the full call sequence — the unit test
        // is proving that the arrow's lexical receiver wins, not
        // that the compiler emits the right opcode (the engine
        // suite's `arrow-this.ts` covers the latter).
        use std::rc::Rc;
        let main = Function {
            id: 0,
            name: "<main>".to_string(),
            span: (0, 0),
            locals: 0,
            scratch: 1,
            param_count: 0,
            own_upvalue_count: 0,
            is_arrow: false,
            has_rest: false,
            code: vec![Instruction {
                pc: 0,
                op: Op::ReturnUndefined,
                operands: vec![],
            }],
            spans: vec![SpanEntry {
                pc: 0,
                span: (0, 0),
            }],
        };
        let arrow = Function {
            id: 1,
            name: "<arrow>".to_string(),
            span: (0, 0),
            locals: 0,
            scratch: 1,
            param_count: 0,
            own_upvalue_count: 0,
            is_arrow: true,
            has_rest: false,
            code: vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadThis,
                    operands: vec![Operand::Register(0)],
                },
                Instruction {
                    pc: 1,
                    op: Op::ReturnValue,
                    operands: vec![Operand::Register(0)],
                },
            ],
            spans: vec![SpanEntry {
                pc: 0,
                span: (0, 0),
            }],
        };
        let module = BytecodeModule {
            module: "arrow.ts".to_string(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![main, arrow],
            constants: vec![],
        };
        // Build the closure by hand and dispatch via `invoke`. The
        // bound_this is a marker string — if `LoadThis` returns it,
        // the lexical override is working.
        let interp = Interpreter::new();
        let bound = JsString::from_str("outer", interp.string_heap()).unwrap();
        let closure = Value::Closure {
            function_id: 1,
            upvalues: Rc::from(Vec::new()),
            bound_this: Some(Box::new(Value::String(bound.clone()))),
        };
        let mut stack: SmallVec<[Frame; 8]> = SmallVec::new();
        stack.push(Frame::for_function(&module.functions[0]));
        // Reserve a scratch slot in <main> to receive the result.
        stack[0].registers.push(Value::Undefined);
        // Caller-supplied this is `Null` — the closure must override.
        interp
            .invoke(
                &mut stack,
                &module,
                &closure,
                Value::Null,
                SmallVec::new(),
                /* dst */ 0,
            )
            .unwrap();
        // Drive the arrow's body to completion, then read r0 of <main>.
        loop {
            let top = stack.len() - 1;
            let f = module
                .functions
                .get(stack[top].function_id as usize)
                .unwrap();
            let pc = stack[top].pc as usize;
            let instr = &f.code[pc];
            if matches!(instr.op, Op::ReturnValue) {
                let value = stack[top].registers[0].clone();
                stack.pop();
                let caller = stack.last_mut().unwrap();
                let dst = caller.return_register.unwrap_or(0) as usize;
                caller.registers[dst] = value;
                break;
            }
            if matches!(instr.op, Op::LoadThis) {
                let dst = match instr.operands[0] {
                    Operand::Register(r) => r,
                    _ => unreachable!(),
                };
                let value = stack[top].this_value.clone();
                stack[top].registers[dst as usize] = value;
                stack[top].pc += 1;
                continue;
            }
            unreachable!();
        }
        assert_eq!(stack[0].registers[0], Value::String(bound));
    }

    #[test]
    fn interrupt_handle_breaks_loop() {
        let module = module_with(
            vec![
                Instruction {
                    pc: 0,
                    op: Op::Nop,
                    operands: vec![],
                },
                Instruction {
                    pc: 1,
                    op: Op::Return,
                    operands: vec![Operand::Register(0)],
                },
            ],
            1,
        );
        let interp = Interpreter::new();
        let handle = interp.interrupt_handle();
        handle.interrupt();
        assert_eq!(interp.run(&module).unwrap_err().error, VmError::Interrupted);
    }
}
