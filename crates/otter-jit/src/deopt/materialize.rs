//! Value materialization — reconstruct interpreter state from JIT frame on deopt.
//!
//! When JIT code bails out, the interpreter needs a complete register window
//! with all live values in their boxed (NaN-boxed) form. The JIT may have
//! values in unboxed form (raw i32, raw f64) or in different registers than
//! the interpreter expects.
//!
//! ## Materialization commands (V8 TranslationArray pattern)
//!
//! Each deopt point has a list of `MaterializeCommand`s describing how to
//! recover each interpreter register from the JIT state:
//!
//! - `Register(jit_reg)`: value is in a JIT register
//! - `StackSlot(offset)`: value is spilled to the JIT stack
//! - `Constant(bits)`: value is a known constant
//! - `BoxedInt32(jit_reg)`: value is an unboxed i32 that needs boxing
//! - `BoxedFloat64(jit_reg)`: value is an unboxed f64 that needs boxing
//! - `Undefined`: value is undefined
//!
//! V8: `TranslationArray` with REGISTER, STACK_SLOT, LITERAL, CAPTURED_OBJECT commands.
//! JSC: `VariableEventStream` producing `Operands<ValueRecovery>`.
//! SM: `SnapshotIterator` reads `LSnapshot` mapping MIR operands to physical locations.
//!
//! Spec: Phase 5.1-5.2 of JIT_INCREMENTAL_PLAN.md

/// How to materialize a single value during deoptimization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializeCommand {
    /// Value is in the interpreter register window at this index (already boxed).
    Register(u16),
    /// Value is a known NaN-boxed constant.
    Constant(u64),
    /// Value is an unboxed i32 in the register window — needs boxing.
    BoxInt32(u16),
    /// Value is an unboxed f64 in the register window — needs boxing.
    BoxFloat64(u16),
    /// Value is undefined.
    Undefined,
    /// Value is null.
    Null,
    /// Value is boolean true.
    True,
    /// Value is boolean false.
    False,
}

/// Frame state snapshot at a deopt point.
///
/// Captures how to reconstruct the interpreter's register window from
/// the JIT's state at the moment of deoptimization.
#[derive(Debug, Clone)]
pub struct FrameStateSnapshot {
    /// Bytecode PC to resume at in the interpreter.
    pub resume_pc: u32,
    /// Commands to materialize each interpreter register.
    /// Index = interpreter register index, value = how to recover it.
    pub registers: Vec<MaterializeCommand>,
    /// The `this` value (NaN-boxed).
    pub this_value: u64,
}

impl FrameStateSnapshot {
    /// Create a snapshot where all registers come directly from the JIT window.
    ///
    /// This is the simplest case: JIT and interpreter share the same register
    /// layout, and all values are in boxed form.
    #[must_use]
    pub fn identity(resume_pc: u32, register_count: u16, this_value: u64) -> Self {
        let registers = (0..register_count)
            .map(MaterializeCommand::Register)
            .collect();
        Self {
            resume_pc,
            registers,
            this_value,
        }
    }

    /// Create an empty snapshot (for functions with no live state).
    #[must_use]
    pub fn empty(resume_pc: u32) -> Self {
        Self {
            resume_pc,
            registers: Vec::new(),
            this_value: 0x7FF8_0000_0000_0000, // undefined
        }
    }

    /// Materialize the interpreter register window from JIT state.
    ///
    /// `jit_registers` is the JIT's register window (raw u64 values).
    /// Returns the materialized interpreter register window (NaN-boxed).
    #[must_use]
    pub fn materialize(&self, jit_registers: &[u64]) -> Vec<u64> {
        self.registers
            .iter()
            .map(|cmd| match cmd {
                MaterializeCommand::Register(idx) => {
                    jit_registers.get(*idx as usize).copied().unwrap_or(TAG_UNDEFINED)
                }
                MaterializeCommand::Constant(bits) => *bits,
                MaterializeCommand::BoxInt32(idx) => {
                    let raw = jit_registers.get(*idx as usize).copied().unwrap_or(0);
                    // Box: TAG_INT32 | (raw as u32)
                    TAG_INT32 | (raw & 0xFFFF_FFFF)
                }
                MaterializeCommand::BoxFloat64(idx) => {
                    // f64 is already stored as its bit pattern — just return it.
                    jit_registers.get(*idx as usize).copied().unwrap_or(0)
                }
                MaterializeCommand::Undefined => TAG_UNDEFINED,
                MaterializeCommand::Null => TAG_NULL,
                MaterializeCommand::True => TAG_TRUE,
                MaterializeCommand::False => TAG_FALSE,
            })
            .collect()
    }
}

// NaN-boxing constants.
const TAG_UNDEFINED: u64 = 0x7FF8_0000_0000_0000;
const TAG_NULL: u64 = 0x7FF8_0000_0000_0001;
const TAG_TRUE: u64 = 0x7FF8_0000_0000_0002;
const TAG_FALSE: u64 = 0x7FF8_0000_0000_0003;
const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_snapshot() {
        let snap = FrameStateSnapshot::identity(42, 3, TAG_UNDEFINED);
        assert_eq!(snap.resume_pc, 42);
        assert_eq!(snap.registers.len(), 3);
        assert_eq!(snap.registers[0], MaterializeCommand::Register(0));
        assert_eq!(snap.registers[1], MaterializeCommand::Register(1));
        assert_eq!(snap.registers[2], MaterializeCommand::Register(2));
    }

    #[test]
    fn test_materialize_identity() {
        let snap = FrameStateSnapshot::identity(0, 3, TAG_UNDEFINED);
        let jit_regs = vec![100u64, 200, 300];
        let result = snap.materialize(&jit_regs);
        assert_eq!(result, vec![100, 200, 300]);
    }

    #[test]
    fn test_materialize_with_constants() {
        let snap = FrameStateSnapshot {
            resume_pc: 10,
            registers: vec![
                MaterializeCommand::Constant(42),
                MaterializeCommand::Undefined,
                MaterializeCommand::True,
            ],
            this_value: TAG_UNDEFINED,
        };
        let result = snap.materialize(&[]);
        assert_eq!(result[0], 42);
        assert_eq!(result[1], TAG_UNDEFINED);
        assert_eq!(result[2], TAG_TRUE);
    }

    #[test]
    fn test_materialize_box_int32() {
        let snap = FrameStateSnapshot {
            resume_pc: 5,
            registers: vec![MaterializeCommand::BoxInt32(0)],
            this_value: TAG_UNDEFINED,
        };
        // JIT register 0 has raw i32 value 99 (unboxed).
        let jit_regs = vec![99u64];
        let result = snap.materialize(&jit_regs);
        // Should be NaN-boxed: TAG_INT32 | 99
        assert_eq!(result[0], TAG_INT32 | 99);
    }

    #[test]
    fn test_materialize_out_of_bounds() {
        let snap = FrameStateSnapshot {
            resume_pc: 0,
            registers: vec![MaterializeCommand::Register(99)], // Out of bounds.
            this_value: TAG_UNDEFINED,
        };
        let jit_regs = vec![42u64];
        let result = snap.materialize(&jit_regs);
        // Out of bounds → undefined.
        assert_eq!(result[0], TAG_UNDEFINED);
    }
}
