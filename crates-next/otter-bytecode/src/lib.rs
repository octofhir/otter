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
}

impl Op {
    /// Canonical mnemonic spelling for disassembly and trace events.
    #[must_use]
    pub const fn mnemonic(self) -> &'static str {
        match self {
            Op::Nop => "NOP",
            Op::LoadUndefined => "LOAD_UNDEFINED",
            Op::Return => "RETURN",
        }
    }

    /// Number of u16 operands the opcode reads after the opcode byte.
    #[must_use]
    pub const fn operand_count(self) -> usize {
        match self {
            Op::Nop => 0,
            Op::LoadUndefined => 1, // dst
            Op::Return => 1,        // src
        }
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

/// Top-level bytecode container produced by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BytecodeModule {
    /// Module specifier (origin path or virtual name).
    pub module: String,
    /// JavaScript or TypeScript.
    pub source_kind: SourceKind,
    /// Function table; index 0 is `<main>`.
    pub functions: Vec<Function>,
}

impl BytecodeModule {
    /// Convenience accessor for `<main>`.
    #[must_use]
    pub fn main(&self) -> &Function {
        &self.functions[0]
    }
}
