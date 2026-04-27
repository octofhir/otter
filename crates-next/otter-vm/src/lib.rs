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
pub mod intrinsics;
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
        /// `MakeFunction` for closure-less functions and reserves
        /// `MakeClosure` for the capture path.
        upvalues: std::rc::Rc<[UpvalueCell]>,
    },
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
            Value::String(s) => s.to_lossy_string(),
            Value::Function { function_id } | Value::Closure { function_id, .. } => {
                format!("[Function #{function_id}]")
            }
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
            Value::String(s) => !s.is_empty(),
            Value::Function { .. } | Value::Closure { .. } | Value::Object(_) | Value::Array(_) => {
                true
            }
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
            (Value::String(a), Value::String(b)) => a.equals(b),
            (Value::Object(a), Value::Object(b)) => a.ptr_eq(b),
            (Value::Array(a), Value::Array(b)) => a.ptr_eq(b),
            (Value::Function { function_id: a }, Value::Function { function_id: b }) => a == b,
            (
                Value::Closure {
                    function_id: a,
                    upvalues: ua,
                },
                Value::Closure {
                    function_id: b,
                    upvalues: ub,
                },
            ) => a == b && std::rc::Rc::ptr_eq(ua, ub),
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
}

impl Frame {
    /// Allocate a frame for `function`. Registers are pre-filled
    /// with `Value::Undefined`. Used for `<main>` (return register
    /// = `None`).
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

    /// Allocate a frame and bind captured upvalues. Used by
    /// `MakeClosure`-driven calls. The function's own captured
    /// locals are appended after the inherited parent upvalues — see
    /// [`Op::MakeClosure`](otter_bytecode::Op::MakeClosure) for the
    /// layout.
    #[must_use]
    pub fn with_return_and_upvalues(
        function: &Function,
        return_register: Option<u16>,
        parent_upvalues: std::rc::Rc<[UpvalueCell]>,
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
                Op::Return | Op::ReturnValue | Op::ReturnUndefined | Op::Call => {
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
                    write_register(
                        frame,
                        dst,
                        Value::Closure {
                            function_id,
                            upvalues,
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
                    let obj = match read_register(frame, obj_reg)? {
                        Value::Object(o) => o.clone(),
                        _ => return Err(VmError::TypeMismatch),
                    };
                    obj.set(&name, value);
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
                    let proto = match read_register(frame, proto_reg)? {
                        Value::Object(p) => Some(p.clone()),
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
                    self.run_numeric(&operands, frame, number::sub)?;
                }
                Op::Mul => {
                    self.run_numeric(&operands, frame, number::mul)?;
                }
                Op::Div => {
                    self.run_numeric(&operands, frame, number::div)?;
                }
                Op::Rem => {
                    self.run_numeric(&operands, frame, number::rem)?;
                }
                Op::Neg => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let n = read_register(frame, src)?
                        .as_number()
                        .ok_or(VmError::TypeMismatch)?;
                    write_register(frame, dst, Value::Number(number::neg(n)))?;
                    frame.pc += 1;
                }
                Op::ToNumber => {
                    let dst = register_operand(operands.first())?;
                    let src = register_operand(operands.get(1))?;
                    let value = match read_register(frame, src)? {
                        Value::Number(n) => *n,
                        Value::Boolean(true) => NumberValue::Smi(1),
                        Value::Boolean(false) | Value::Null => NumberValue::Smi(0),
                        Value::Undefined
                        | Value::Function { .. }
                        | Value::Closure { .. }
                        | Value::Object(_)
                        | Value::Array(_) => NumberValue::Double(f64::NAN),
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
                Op::CallMethod => {
                    let dst = register_operand(operands.first())?;
                    let recv = register_operand(operands.get(1))?;
                    let name_idx = const_operand(operands.get(2))?;
                    let argc = match operands.get(3) {
                        Some(&Operand::ConstIndex(n)) => n,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let name = match module.constants.get(name_idx as usize) {
                        Some(Constant::String { utf16 }) => String::from_utf16_lossy(utf16),
                        _ => return Err(VmError::InvalidOperand),
                    };
                    // Collect argument registers into a SmallVec to
                    // avoid `Vec` allocation for the common case.
                    let mut arg_values: SmallVec<[Value; 4]> =
                        SmallVec::with_capacity(argc as usize);
                    for i in 0..argc as usize {
                        let r = register_operand(operands.get(4 + i))?;
                        arg_values.push(read_register(frame, r)?.clone());
                    }
                    let recv_value = read_register(frame, recv)?.clone();
                    // Dispatch by receiver kind. Each prototype
                    // registry returns `None` for unknown method
                    // names, falling through to the next branch
                    // before raising `UnknownIntrinsic`.
                    let entry = match &recv_value {
                        Value::String(_) => string_prototype::lookup(&name),
                        Value::Array(_) => array_prototype::lookup(&name),
                        _ => None,
                    }
                    .ok_or_else(|| VmError::UnknownIntrinsic { name: name.clone() })?;
                    let result = (entry.impl_fn)(&IntrinsicArgs {
                        receiver: &recv_value,
                        args: &arg_values,
                        string_heap: &self.string_heap,
                    })
                    .map_err(intrinsic_to_vm_error)?;
                    write_register(frame, dst, result)?;
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
        let Some(return_reg) = popped.return_register else {
            return Ok(Some(value));
        };
        let caller = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(caller, return_reg, value)?;
        // Caller's pc was set to the next instruction at call time;
        // nothing to advance here.
        Ok(None)
    }

    /// Handle `Op::Call`: push a new frame for the callee with
    /// arguments copied into the parameter slots.
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
        let caller = &mut stack[top_idx];
        let (function_id, parent_upvalues) = match read_register(caller, callee_reg)? {
            Value::Function { function_id } => (*function_id, std::rc::Rc::from(Vec::new())),
            Value::Closure {
                function_id,
                upvalues,
            } => (*function_id, upvalues.clone()),
            _ => return Err(VmError::NotCallable),
        };
        // Collect args from caller registers — bounded by argc, so
        // a `SmallVec<[Value; 8]>` keeps the small-call path off
        // the heap.
        let mut args: SmallVec<[Value; 8]> = SmallVec::with_capacity(argc as usize);
        for i in 0..argc as usize {
            let r = register_operand(operands.get(3 + i))?;
            args.push(read_register(caller, r)?.clone());
        }
        // Advance caller's pc so the post-call dispatch resumes
        // after the call instruction.
        caller.pc = caller.pc.checked_add(1).ok_or(VmError::InvalidOperand)?;

        if stack.len() as u32 >= self.max_stack_depth {
            return Err(VmError::StackOverflow {
                limit: self.max_stack_depth,
            });
        }
        let function = module
            .functions
            .get(function_id as usize)
            .ok_or(VmError::InvalidOperand)?;
        let mut new_frame = Frame::with_return_and_upvalues(function, Some(dst), parent_upvalues);
        // Bind parameters: extra args are dropped, missing args
        // stay `Value::Undefined` (matches JS semantics).
        let bind_count = (function.param_count as usize).min(args.len());
        for (i, value) in args.into_iter().take(bind_count).enumerate() {
            let slot = new_frame
                .registers
                .get_mut(i)
                .ok_or(VmError::InvalidOperand)?;
            *slot = value;
        }
        stack.push(new_frame);
        Ok(())
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
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = self.binop_regs(operands, frame)?;
        let l = lhs.as_number().ok_or(VmError::TypeMismatch)?;
        let r = rhs.as_number().ok_or(VmError::TypeMismatch)?;
        write_register(frame, dst, Value::Number(op(l, r)))?;
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
                let ord = number::compare(*a, *b);
                match (op, ord) {
                    (_, NumericOrdering::Unordered) => false,
                    (Op::LessThan, NumericOrdering::Less) => true,
                    (Op::LessEq, NumericOrdering::Less | NumericOrdering::Equal) => true,
                    (Op::GreaterThan, NumericOrdering::Greater) => true,
                    (Op::GreaterEq, NumericOrdering::Greater | NumericOrdering::Equal) => true,
                    _ => false,
                }
            }
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
