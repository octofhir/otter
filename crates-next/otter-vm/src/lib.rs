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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use otter_bytecode::{BytecodeModule, Function, Op, Operand};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Foundation value model placeholder.
///
/// Slice `09` (string core) replaces this with a real tagged
/// representation. For the harness slice (task 07), only
/// `Undefined` exists.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Value {
    /// The JS `undefined` value.
    Undefined,
}

impl Value {
    /// Render the value as a debug-style string suitable for CLI
    /// preview output. The harness only ever produces
    /// `"undefined"`.
    #[must_use]
    pub fn display_string(&self) -> String {
        match self {
            Value::Undefined => "undefined".to_string(),
        }
    }
}

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
    /// `InterruptFlag` was tripped before the next checkpoint.
    Interrupted,
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::MissingReturn => write!(f, "function did not RETURN"),
            VmError::InvalidOperand => write!(f, "invalid operand"),
            VmError::Interrupted => write!(f, "interrupted"),
        }
    }
}

impl std::error::Error for VmError {}

/// Match-based dispatch loop. The harness baseline; slice tasks may
/// later switch to threaded dispatch after benchmark-driven review
/// (foundation plan §"Interpreter requirements").
#[derive(Debug)]
pub struct Interpreter {
    interrupt: InterruptFlag,
}

impl Interpreter {
    /// Construct a fresh interpreter with its own interrupt flag.
    #[must_use]
    pub fn new() -> Self {
        Self {
            interrupt: InterruptFlag::new(),
        }
    }

    /// Cloneable handle for cooperative cancellation.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptFlag {
        self.interrupt.clone()
    }

    /// Execute `<main>` of `module` and return its completion value.
    ///
    /// # Errors
    /// Returns [`VmError`] on bytecode malformation or interrupt.
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
                    let slot = frame
                        .registers
                        .get_mut(dst as usize)
                        .ok_or(VmError::InvalidOperand)?;
                    *slot = Value::Undefined;
                    frame.pc += 1;
                }
                Op::Return => {
                    let src = register_operand(instr.operands.first())?;
                    let value = frame
                        .registers
                        .get(src as usize)
                        .copied()
                        .ok_or(VmError::InvalidOperand)?;
                    return Ok(value);
                }
            }
        }
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
