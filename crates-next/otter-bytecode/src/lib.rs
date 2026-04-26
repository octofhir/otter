//! Otter foundation bytecode: container, opcode set, encoding, and dumps.
//!
//! This crate is the single source of truth for the new engine's
//! bytecode shape. It is consumed by `otter-compiler` (writers) and
//! `otter-vm` (readers / executors). It does **not** execute anything.
//!
//! # Contents
//! - [`Op`] — canonical opcode enum (`Nop`, `LoadUndefined`, `Return`
//!   for the harness slice; extended slice-by-slice).
//! - [`Instruction`] — decoded form: `(pc, op, operands)`.
//! - [`Function`] — one compiled function: registers, code, spans,
//!   constants index.
//! - [`BytecodeModule`] — top-level container the compiler emits and
//!   the VM consumes.
//! - [`disasm`] — text disassembler per spec
//!   [`docs/new-engine/specs/bytecode-dump-disasm-trace.md`](
//!     ../../../docs/new-engine/specs/bytecode-dump-disasm-trace.md
//!   ).
//! - [`dump`] — JSON dump per the same spec
//!   (`otterBytecodeDumpVersion: 1`).
//!
//! # Invariants
//! - Instructions inside [`Function::code`] are sorted by `pc`
//!   ascending; spans inside [`Function::spans`] are sorted by `pc`.
//! - Mnemonics are `SCREAMING_SNAKE_CASE` and match the strings the
//!   disassembler emits.
//!
//! # See also
//! - [`docs/new-engine/specs/bytecode-dump-disasm-trace.md`](
//!     ../../../docs/new-engine/specs/bytecode-dump-disasm-trace.md
//!   )
//! - [`docs/new-engine/adr/0003-public-api-and-cli.md`](
//!     ../../../docs/new-engine/adr/0003-public-api-and-cli.md
//!   )

pub mod disasm;
pub mod dump;

use serde::{Deserialize, Serialize};

/// The canonical foundation opcode set.
///
/// The harness slice (task 07) provides only the three opcodes
/// required to compile and execute the smoke fixtures
/// (`empty-script.ts`, `literal-undefined.ts`). Slice tasks
/// `09`–`13` extend this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// No operation. Used as a placeholder; cost: one dispatch tick.
    Nop,
    /// `r<dst> = undefined`.
    LoadUndefined,
    /// Return from the current function with `r<src>` as the
    /// completion value.
    Return,
    /// `r<dst> = constants[k<idx>]` (string constant).
    LoadString,
    /// `r<dst> = constants[k<idx>]` (number constant).
    LoadNumber,
    /// `r<dst> = imm:i32` (small-integer immediate via
    /// `Operand::ConstIndex` — the constant pool holds the literal).
    LoadInt32,
    /// `r<dst> = true`.
    LoadTrue,
    /// `r<dst> = false`.
    LoadFalse,
    /// `r<dst> = r<src>.length` (string operand). Returns Number.
    LoadLength,
    /// `r<dst> = r<recv>[r<idx>]` for string operand. Out-of-range
    /// yields the empty string.
    GetStringIndex,
    /// Variadic method call dispatched through the
    /// `String.prototype` intrinsic table. Operands:
    /// `dst, recv, name_const, argc, args...`.
    CallStringMethod,

    // Polymorphic binary operators. Operands: `dst, lhs, rhs`.
    // Handle Number+Number and String+String operand pairs;
    // mixed types raise `TypeMismatch` until later slices add
    // coercion.
    /// `r<dst> = r<lhs> + r<rhs>` (Number+Number or String+String).
    Add,
    /// `r<dst> = r<lhs> - r<rhs>` (Number+Number).
    Sub,
    /// `r<dst> = r<lhs> * r<rhs>` (Number+Number).
    Mul,
    /// `r<dst> = r<lhs> / r<rhs>` (Number+Number).
    Div,
    /// `r<dst> = r<lhs> % r<rhs>` (Number+Number).
    Rem,
    /// `r<dst> = -r<src>` (Number).
    Neg,
    /// `r<dst> = ToNumber(r<src>)` (foundation subset).
    ToNumber,
    /// `r<dst> = (r<lhs> === r<rhs>)`. Returns Boolean.
    Equal,
    /// `r<dst> = (r<lhs> !== r<rhs>)`. Returns Boolean.
    NotEqual,
    /// `r<dst> = (r<lhs> < r<rhs>)`. Number+Number or String+String.
    LessThan,
    /// `r<dst> = (r<lhs> <= r<rhs>)`.
    LessEq,
    /// `r<dst> = (r<lhs> > r<rhs>)`.
    GreaterThan,
    /// `r<dst> = (r<lhs> >= r<rhs>)`.
    GreaterEq,

    /// `r<dst> = null`.
    LoadNull,
    /// `r<dst> = !ToBoolean(r<src>)`.
    LogicalNot,
    /// `r<dst> = ToBoolean(r<src>)` — explicit coercion used by
    /// branch operands the compiler cannot statically prove are
    /// boolean.
    ToBoolean,
    /// Unconditional relative branch: `pc += imm32(rel)`.
    /// Operand: `Imm32(signed_offset)`. Offset is relative to the
    /// **next** instruction.
    Jump,
    /// Branch when `ToBoolean(r<cond>)` is true. Operands:
    /// `Imm32(signed_offset), Register(cond)`.
    JumpIfTrue,
    /// Branch when `ToBoolean(r<cond>)` is false.
    JumpIfFalse,
    /// Branch when `r<cond>` is `null` or `undefined`. Used for
    /// nullish coalescing `??`.
    JumpIfNullish,
    /// `r<dst> = locals[idx]`. Operands:
    /// `Register(dst), Imm32(local_index)`.
    LoadLocal,
    /// `locals[idx] = r<src>`. Operands:
    /// `Register(src), Imm32(local_index)`.
    StoreLocal,
    /// Throw a `ReferenceError` for a TDZ-violating local read.
    /// Operand: `Imm32(local_index)`. Used until full lexical
    /// environments arrive.
    TdzError,

    /// `r<dst> = function-value(constants[k<idx>])`. The constant
    /// is a [`Constant::FunctionId`] referencing
    /// [`BytecodeModule::functions`].
    MakeFunction,
    /// Variadic call. Operands: `dst, callee, argc, args...`. The
    /// callee must be a function value at this slice.
    Call,
    /// Return `r<src>` from the current function. Reuses
    /// [`Op::Return`] semantics in `<main>`; in nested calls the
    /// dispatcher pops the frame and writes the value into the
    /// caller's `return_register`.
    ReturnValue,
    /// Return `undefined` from the current function. Convenience
    /// emitted at fall-through end of function bodies.
    ReturnUndefined,

    /// `r<dst> = new JsObject()`. Operand: `dst`.
    NewObject,
    /// `r<dst> = r<obj>.<name>`. Operands: `dst, obj, name_const`.
    /// Missing property reads as `undefined`. Non-object receivers
    /// raise `TypeMismatch`.
    LoadProperty,
    /// `r<obj>.<name> = r<src>`. Operands: `obj, name_const, src`.
    StoreProperty,
    /// `r<dst> = delete r<obj>.<name>` (boolean result).
    /// Operands: `dst, obj, name_const`.
    DeleteProperty,
    /// `r<dst> = Object.getPrototypeOf(r<obj>)`. Operands:
    /// `dst, obj`. Returns `null` when no prototype is set;
    /// raises `TypeMismatch` for non-object receivers.
    GetPrototype,
    /// `Object.setPrototypeOf(r<obj>, r<proto>)`. Operands:
    /// `obj, proto`. `proto` may be a `Value::Object` or
    /// `Value::Null`. Other types raise `TypeMismatch`.
    SetPrototype,
    /// Build a fresh dense array from `elem_count` register
    /// operands. Operands: `dst, count, elem0, elem1, …`.
    NewArray,
    /// `r<dst> = r<arr>[r<idx>]`. Operands: `dst, arr, idx`.
    /// `arr` must be `Value::Array`; `idx` must be `Value::Number`
    /// in `[0, u32::MAX]` (truncates to `u32`).
    LoadElement,
    /// `r<arr>[r<idx>] = r<src>`. Operands: `arr, idx, src`.
    StoreElement,
    /// `r<dst> = r<arr>.length`. Operands: `dst, arr`.
    ArrayLength,
    /// `r<dst> = (r<lhs> instanceof r<rhs>)`. Operands:
    /// `dst, lhs, rhs`. Foundation slice 19 semantics:
    ///
    /// - `rhs` carries a `prototype` property (set later by class
    ///   lowering): the runtime walks `lhs`'s prototype chain
    ///   looking for `rhs.prototype`.
    /// - When `rhs` is itself a plain object, the runtime treats
    ///   it as the "prototype to find" and walks `lhs`'s chain
    ///   looking for it directly. This keeps the opcode useful
    ///   before classes land.
    /// - Anything else returns `false`.
    Instanceof,
}

impl Op {
    /// Canonical mnemonic spelling for disassembly and trace events.
    #[must_use]
    pub const fn mnemonic(self) -> &'static str {
        match self {
            Op::Nop => "NOP",
            Op::LoadUndefined => "LOAD_UNDEFINED",
            Op::Return => "RETURN",
            Op::LoadString => "LOAD_STRING",
            Op::LoadNumber => "LOAD_NUMBER",
            Op::LoadInt32 => "LOAD_INT32",
            Op::LoadTrue => "LOAD_TRUE",
            Op::LoadFalse => "LOAD_FALSE",
            Op::LoadLength => "LOAD_LENGTH",
            Op::GetStringIndex => "GET_STRING_INDEX",
            Op::CallStringMethod => "CALL_STRING_METHOD",
            Op::Add => "ADD",
            Op::Sub => "SUB",
            Op::Mul => "MUL",
            Op::Div => "DIV",
            Op::Rem => "REM",
            Op::Neg => "NEG",
            Op::ToNumber => "TO_NUMBER",
            Op::Equal => "EQ",
            Op::NotEqual => "NEQ",
            Op::LessThan => "LT",
            Op::LessEq => "LE",
            Op::GreaterThan => "GT",
            Op::GreaterEq => "GE",
            Op::LoadNull => "LOAD_NULL",
            Op::LogicalNot => "NOT",
            Op::ToBoolean => "TO_BOOLEAN",
            Op::Jump => "JUMP",
            Op::JumpIfTrue => "JUMP_IF_TRUE",
            Op::JumpIfFalse => "JUMP_IF_FALSE",
            Op::JumpIfNullish => "JUMP_IF_NULLISH",
            Op::LoadLocal => "LOAD_LOCAL",
            Op::StoreLocal => "STORE_LOCAL",
            Op::TdzError => "TDZ_ERROR",
            Op::MakeFunction => "MAKE_FUNCTION",
            Op::Call => "CALL",
            Op::ReturnValue => "RETURN_VALUE",
            Op::ReturnUndefined => "RETURN_UNDEFINED",
            Op::NewObject => "NEW_OBJECT",
            Op::LoadProperty => "LOAD_PROPERTY",
            Op::StoreProperty => "STORE_PROPERTY",
            Op::DeleteProperty => "DELETE_PROPERTY",
            Op::GetPrototype => "GET_PROTOTYPE",
            Op::SetPrototype => "SET_PROTOTYPE",
            Op::NewArray => "NEW_ARRAY",
            Op::LoadElement => "LOAD_ELEMENT",
            Op::StoreElement => "STORE_ELEMENT",
            Op::ArrayLength => "ARRAY_LENGTH",
            Op::Instanceof => "INSTANCEOF",
        }
    }

    /// Declared operand arity. `CallStringMethod` is variadic; the
    /// instruction stream stores `dst, recv, name_const, argc`
    /// followed by `argc` register operands, so the actual operand
    /// count is `4 + argc`. `operand_count` returns the **prefix**
    /// length; consumers walk the variadic tail by reading `argc`.
    #[must_use]
    pub const fn operand_count(self) -> usize {
        match self {
            Op::Nop | Op::ReturnUndefined => 0,
            Op::LoadUndefined
            | Op::LoadNull
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::Return
            | Op::ReturnValue
            | Op::Jump
            | Op::TdzError
            | Op::NewObject => 1,
            Op::LoadString
            | Op::LoadNumber
            | Op::LoadInt32
            | Op::LoadLength
            | Op::Neg
            | Op::ToNumber
            | Op::LogicalNot
            | Op::ToBoolean
            | Op::JumpIfTrue
            | Op::JumpIfFalse
            | Op::JumpIfNullish
            | Op::LoadLocal
            | Op::StoreLocal
            | Op::MakeFunction => 2,
            Op::GetStringIndex
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Equal
            | Op::NotEqual
            | Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::LoadProperty
            | Op::StoreProperty
            | Op::DeleteProperty
            | Op::Instanceof => 3,
            Op::GetPrototype | Op::SetPrototype | Op::ArrayLength => 2,
            // `NewArray` is variadic: `dst, count, elems...`. The
            // dispatcher reads the count and walks the trailing
            // operands.
            Op::NewArray => 2,
            Op::LoadElement | Op::StoreElement => 3,
            Op::CallStringMethod => 4, // dst, recv, name_const, argc
            Op::Call => 3,             // dst, callee, argc — args follow
        }
    }

    /// Whether the opcode performs a control-flow transfer. The
    /// dispatcher uses this to advance `pc` by 1 only for non-jump
    /// opcodes; jumps mutate `pc` themselves (and the back-edge
    /// hook polls the interrupt flag).
    #[must_use]
    pub const fn is_branch(self) -> bool {
        matches!(
            self,
            Op::Jump
                | Op::JumpIfTrue
                | Op::JumpIfFalse
                | Op::JumpIfNullish
                | Op::Return
                | Op::ReturnValue
                | Op::ReturnUndefined
        )
    }
}

/// One decoded instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    /// Program counter (byte offset within the function's `code`).
    pub pc: u32,
    /// Opcode.
    pub op: Op,
    /// Operands in declaration order.
    pub operands: Vec<Operand>,
}

/// One operand value with a kind tag for the JSON dump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Operand {
    /// Register index (locals + scratch live in one register window).
    Register(u16),
    /// Index into [`BytecodeModule::constants`].
    ConstIndex(u32),
    /// Inline signed 32-bit immediate (used by `LoadInt32`).
    Imm32(i32),
}

/// One source-span entry attached to a `pc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanEntry {
    /// Program counter.
    pub pc: u32,
    /// Byte offset range into the original source (`(start, end)`).
    pub span: (u32, u32),
}

/// One compiled function.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Function {
    /// Index into `BytecodeModule::functions`.
    pub id: u32,
    /// Display name; falls back to `<main>` for the script entry.
    pub name: String,
    /// Original source span.
    pub span: (u32, u32),
    /// Number of declared local registers.
    pub locals: u16,
    /// Number of scratch registers above the locals.
    pub scratch: u16,
    /// Number of declared parameters. The first `param_count`
    /// register slots are reserved for parameter binding.
    #[serde(default)]
    pub param_count: u16,
    /// Encoded instructions.
    pub code: Vec<Instruction>,
    /// `pc -> source span` table.
    pub spans: Vec<SpanEntry>,
}

/// Source-language flavor (per ADR-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// JavaScript (`.js`, `.mjs`, `.cjs`).
    JavaScript,
    /// TypeScript (`.ts`, `.mts`, `.cts`).
    TypeScript,
}

/// Constant-pool entry referenced by [`Operand::ConstIndex`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Constant {
    /// String constant. Stored as WTF-16 code units to round-trip
    /// surrogates losslessly through the JSON dump.
    String {
        /// WTF-16 code units.
        utf16: Vec<u16>,
    },
    /// Numeric constant stored as raw IEEE-754 bits to round-trip
    /// `NaN`, `±Infinity`, and `-0.0` losslessly through JSON.
    Number {
        /// `f64::to_bits` representation.
        bits: u64,
    },
    /// Reference to [`BytecodeModule::functions`] — a function
    /// declaration / expression captured at compile time.
    FunctionId {
        /// Index into `BytecodeModule::functions`.
        index: u32,
    },
}

/// Top-level bytecode container produced by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BytecodeModule {
    /// Module specifier (origin path or virtual name).
    pub module: String,
    /// JavaScript or TypeScript.
    pub source_kind: SourceKind,
    /// Function table; index 0 is `<main>`.
    pub functions: Vec<Function>,
    /// Module-wide constant pool.
    #[serde(default)]
    pub constants: Vec<Constant>,
}

impl BytecodeModule {
    /// Convenience accessor for `<main>`.
    #[must_use]
    pub fn main(&self) -> &Function {
        &self.functions[0]
    }
}
