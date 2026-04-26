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

pub mod intrinsics;
pub mod number;
pub mod string;
pub mod string_prototype;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use otter_bytecode::{BytecodeModule, Constant, Function, Op, Operand};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::intrinsics::{IntrinsicArgs, IntrinsicError};

pub use number::{NumberValue, NumericOrdering};
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

/// One call frame. Compact and cache-conscious per foundation plan
/// §M7. The harness slice does not yet allocate frames per call —
/// there is exactly one frame for `<main>`.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Index into the bytecode container's function table.
    pub function_id: u32,
    /// Current program counter (instruction index, not byte offset).
    pub pc: u32,
    /// Register window for this frame.
    pub registers: SmallVec<[Value; 8]>,
}

impl Frame {
    /// Allocate a frame for `function`. Registers are pre-filled
    /// with `Value::Undefined`.
    #[must_use]
    pub fn for_function(function: &Function) -> Self {
        let total = (function.locals + function.scratch) as usize;
        let mut registers: SmallVec<[Value; 8]> = SmallVec::with_capacity(total);
        registers.resize(total, Value::Undefined);
        Self {
            function_id: function.id,
            pc: 0,
            registers,
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

/// Match-based dispatch loop. The harness baseline; slice tasks may
/// later switch to threaded dispatch after benchmark-driven review
/// (foundation plan §"Interpreter requirements").
#[derive(Debug)]
pub struct Interpreter {
    interrupt: InterruptFlag,
    string_heap: Arc<StringHeap>,
}

impl Interpreter {
    /// Construct a fresh interpreter with its own interrupt flag
    /// and a no-cap string heap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            interrupt: InterruptFlag::new(),
            string_heap: Arc::new(StringHeap::default()),
        }
    }

    /// Construct an interpreter with a string heap cap (`0` =
    /// unlimited).
    #[must_use]
    pub fn with_string_heap_cap(cap_bytes: u64) -> Self {
        Self {
            interrupt: InterruptFlag::new(),
            string_heap: Arc::new(StringHeap::with_cap(cap_bytes)),
        }
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
    /// Returns [`VmError`] on bytecode malformation, type mismatch,
    /// OOM, or interrupt.
    pub fn run(&self, module: &BytecodeModule) -> Result<Value, VmError> {
        let main = module.main();
        let mut frame = Frame::for_function(main);

        loop {
            if self.interrupt.is_set() {
                return Err(VmError::Interrupted);
            }
            let instr = main
                .code
                .get(frame.pc as usize)
                .ok_or(VmError::MissingReturn)?;
            match instr.op {
                Op::Nop => {
                    frame.pc += 1;
                }
                Op::LoadUndefined => {
                    let dst = register_operand(instr.operands.first())?;
                    write_register(&mut frame, dst, Value::Undefined)?;
                    frame.pc += 1;
                }
                Op::Return => {
                    let src = register_operand(instr.operands.first())?;
                    let value = read_register(&frame, src)?.clone();
                    return Ok(value);
                }
                Op::LoadString => {
                    let dst = register_operand(instr.operands.first())?;
                    let idx = const_operand(instr.operands.get(1))?;
                    let units = match module.constants.get(idx as usize) {
                        Some(otter_bytecode::Constant::String { utf16 }) => utf16.as_slice(),
                        _ => return Err(VmError::InvalidOperand),
                    };
                    let s = JsString::from_utf16_units(units, &self.string_heap)?;
                    write_register(&mut frame, dst, Value::String(s))?;
                    frame.pc += 1;
                }
                Op::LoadLength => {
                    let dst = register_operand(instr.operands.first())?;
                    let src = register_operand(instr.operands.get(1))?;
                    let s = read_register(&frame, src)?
                        .as_string()
                        .ok_or(VmError::TypeMismatch)?;
                    let len = NumberValue::from_i32(s.len() as i32);
                    write_register(&mut frame, dst, Value::Number(len))?;
                    frame.pc += 1;
                }
                Op::LoadNumber => {
                    let dst = register_operand(instr.operands.first())?;
                    let idx = const_operand(instr.operands.get(1))?;
                    let value = match module.constants.get(idx as usize) {
                        Some(Constant::Number { bits }) => {
                            NumberValue::from_f64(f64::from_bits(*bits))
                        }
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(&mut frame, dst, Value::Number(value))?;
                    frame.pc += 1;
                }
                Op::LoadInt32 => {
                    let dst = register_operand(instr.operands.first())?;
                    let imm = match instr.operands.get(1) {
                        Some(Operand::Imm32(v)) => *v,
                        _ => return Err(VmError::InvalidOperand),
                    };
                    write_register(&mut frame, dst, Value::Number(NumberValue::Smi(imm)))?;
                    frame.pc += 1;
                }
                Op::LoadTrue => {
                    let dst = register_operand(instr.operands.first())?;
                    write_register(&mut frame, dst, Value::Boolean(true))?;
                    frame.pc += 1;
                }
                Op::LoadFalse => {
                    let dst = register_operand(instr.operands.first())?;
                    write_register(&mut frame, dst, Value::Boolean(false))?;
                    frame.pc += 1;
                }
                Op::LoadNull => {
                    let dst = register_operand(instr.operands.first())?;
                    write_register(&mut frame, dst, Value::Null)?;
                    frame.pc += 1;
                }
                Op::LogicalNot => {
                    let dst = register_operand(instr.operands.first())?;
                    let src = register_operand(instr.operands.get(1))?;
                    let truthy = read_register(&frame, src)?.to_boolean();
                    write_register(&mut frame, dst, Value::Boolean(!truthy))?;
                    frame.pc += 1;
                }
                Op::ToBoolean => {
                    let dst = register_operand(instr.operands.first())?;
                    let src = register_operand(instr.operands.get(1))?;
                    let truthy = read_register(&frame, src)?.to_boolean();
                    write_register(&mut frame, dst, Value::Boolean(truthy))?;
                    frame.pc += 1;
                }
                Op::Jump => {
                    let offset = imm32_operand(instr.operands.first())?;
                    apply_branch(&mut frame, offset, &self.interrupt)?;
                }
                Op::JumpIfTrue => {
                    let offset = imm32_operand(instr.operands.first())?;
                    let cond = register_operand(instr.operands.get(1))?;
                    if read_register(&frame, cond)?.to_boolean() {
                        apply_branch(&mut frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                }
                Op::JumpIfFalse => {
                    let offset = imm32_operand(instr.operands.first())?;
                    let cond = register_operand(instr.operands.get(1))?;
                    if !read_register(&frame, cond)?.to_boolean() {
                        apply_branch(&mut frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                }
                Op::JumpIfNullish => {
                    let offset = imm32_operand(instr.operands.first())?;
                    let cond = register_operand(instr.operands.get(1))?;
                    if read_register(&frame, cond)?.is_nullish() {
                        apply_branch(&mut frame, offset, &self.interrupt)?;
                    } else {
                        frame.pc += 1;
                    }
                }
                Op::LoadLocal => {
                    let dst = register_operand(instr.operands.first())?;
                    let idx = imm32_operand(instr.operands.get(1))?;
                    let value = read_register(&frame, idx as u16)?.clone();
                    write_register(&mut frame, dst, value)?;
                    frame.pc += 1;
                }
                Op::StoreLocal => {
                    let src = register_operand(instr.operands.first())?;
                    let idx = imm32_operand(instr.operands.get(1))?;
                    let value = read_register(&frame, src)?.clone();
                    write_register(&mut frame, idx as u16, value)?;
                    frame.pc += 1;
                }
                Op::TdzError => {
                    return Err(VmError::TemporalDeadZone {
                        local_index: imm32_operand(instr.operands.first())? as u32,
                    });
                }
                Op::Add => {
                    self.run_add(module, &instr.operands, &mut frame)?;
                }
                Op::Sub => {
                    self.run_numeric(&instr.operands, &mut frame, number::sub)?;
                }
                Op::Mul => {
                    self.run_numeric(&instr.operands, &mut frame, number::mul)?;
                }
                Op::Div => {
                    self.run_numeric(&instr.operands, &mut frame, number::div)?;
                }
                Op::Rem => {
                    self.run_numeric(&instr.operands, &mut frame, number::rem)?;
                }
                Op::Neg => {
                    let dst = register_operand(instr.operands.first())?;
                    let src = register_operand(instr.operands.get(1))?;
                    let n = read_register(&frame, src)?
                        .as_number()
                        .ok_or(VmError::TypeMismatch)?;
                    write_register(&mut frame, dst, Value::Number(number::neg(n)))?;
                    frame.pc += 1;
                }
                Op::ToNumber => {
                    let dst = register_operand(instr.operands.first())?;
                    let src = register_operand(instr.operands.get(1))?;
                    let value = match read_register(&frame, src)? {
                        Value::Number(n) => *n,
                        Value::Boolean(true) => NumberValue::Smi(1),
                        Value::Boolean(false) | Value::Null => NumberValue::Smi(0),
                        Value::Undefined => NumberValue::Double(f64::NAN),
                        Value::String(s) => number::to_number_from_string(&s.to_lossy_string()),
                    };
                    write_register(&mut frame, dst, Value::Number(value))?;
                    frame.pc += 1;
                }
                Op::Equal => {
                    let (dst, lhs, rhs) = self.binop_regs(&instr.operands, &frame)?;
                    let eq = lhs == rhs;
                    write_register(&mut frame, dst, Value::Boolean(eq))?;
                    frame.pc += 1;
                }
                Op::NotEqual => {
                    let (dst, lhs, rhs) = self.binop_regs(&instr.operands, &frame)?;
                    let eq = lhs == rhs;
                    write_register(&mut frame, dst, Value::Boolean(!eq))?;
                    frame.pc += 1;
                }
                Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
                    self.run_compare(&instr.operands, &mut frame, instr.op)?;
                }
                Op::GetStringIndex => {
                    let dst = register_operand(instr.operands.first())?;
                    let recv = register_operand(instr.operands.get(1))?;
                    let idx_reg = register_operand(instr.operands.get(2))?;
                    let recv_s = read_register(&frame, recv)?
                        .as_string()
                        .ok_or(VmError::TypeMismatch)?
                        .clone();
                    let idx = match read_register(&frame, idx_reg)? {
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
                    write_register(&mut frame, dst, Value::String(result_str))?;
                    frame.pc += 1;
                }
                Op::CallStringMethod => {
                    let dst = register_operand(instr.operands.first())?;
                    let recv = register_operand(instr.operands.get(1))?;
                    let name_idx = const_operand(instr.operands.get(2))?;
                    let argc = match instr.operands.get(3) {
                        Some(Operand::ConstIndex(n)) => *n,
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
                        let r = register_operand(instr.operands.get(4 + i))?;
                        arg_values.push(read_register(&frame, r)?.clone());
                    }
                    let recv_value = read_register(&frame, recv)?.clone();
                    let entry = string_prototype::lookup(&name)
                        .ok_or_else(|| VmError::UnknownIntrinsic { name: name.clone() })?;
                    let result = (entry.impl_fn)(&IntrinsicArgs {
                        receiver: &recv_value,
                        args: &arg_values,
                        string_heap: &self.string_heap,
                    })
                    .map_err(intrinsic_to_vm_error)?;
                    write_register(&mut frame, dst, result)?;
                    frame.pc += 1;
                }
            }
        }
    }
}

impl Interpreter {
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
        assert_eq!(interp.run(&module).unwrap_err(), VmError::MissingReturn);
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
        assert_eq!(interp.run(&module).unwrap_err(), VmError::Interrupted);
    }
}
