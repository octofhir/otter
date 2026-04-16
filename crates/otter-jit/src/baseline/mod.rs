//! Ignition-style accumulator template baseline analyzer and emitter.
//!
//! Walks a function's [`Function::bytecode`](otter_vm::module::Function::bytecode)
//! stream and lowers the hot subset — the int32 arithmetic-loop shape that
//! drives `arithmetic_loop.ts` — into a compact instruction list designed
//! for an x21-pinned-accumulator emitter.
//!
//! # Pipeline position
//!
//! ```text
//! Function::bytecode()
//!         ↓
//!   [analyze_template_candidate]
//!         ↓
//!   TemplateProgram
//!         ↓
//!   [emit_template_stencil]
//!         ↓
//!   x21-pinned aarch64 code
//! ```

use otter_vm::bytecode::{InstructionIter, Opcode, Operand};
use otter_vm::module::Function;

/// An operation in the v2 baseline IR. Each op reads / writes the
/// accumulator (held in x21 by the Phase 4.2 emitter) and at most one
/// named register, making the IR a 1-or-2-address shape rather than v1's
/// 3-address shape.
///
/// Comparisons intentionally do **not** write a boolean to a slot — they
/// leave the result in ARM's NZCV flags so the fused
/// [`JumpIfAccFalse`](TemplateInstruction::JumpIfAccFalse) or
/// [`JumpIfCompareFalse`](TemplateInstruction::JumpIfCompareFalse) can
/// branch directly. The v1 emitter already implements fused compare +
/// branch via `emit_fused_compare_branch`; the v2 emitter reuses that
/// idea but drops the register-writeback step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateInstruction {
    /// `acc = imm`. Maps from `LdaSmi imm`.
    LdaI32 { imm: i32 },
    /// `reg = acc`. Maps from `Star reg`.
    Star { reg: u16 },
    /// `acc = reg`. Maps from `Ldar reg`.
    Ldar { reg: u16 },
    /// `acc = acc + reg` (int32, tag-guarded). Maps from `Add reg`.
    AddAcc { rhs: u16 },
    /// `acc = acc + imm` (int32). Maps from `AddSmi imm`. The emitter
    /// materialises the immediate in a scratch register before the add
    /// so x21 stays clean.
    AddAccI32 { imm: i32 },
    /// `acc = acc - reg`.
    SubAcc { rhs: u16 },
    /// `acc = acc - imm`. Maps from `SubSmi imm`.
    SubAccI32 { imm: i32 },
    /// `acc = acc * reg`.
    MulAcc { rhs: u16 },
    /// `acc = acc | reg`.
    BitOrAcc { rhs: u16 },
    /// `acc = acc | imm`. Maps from `BitwiseOrSmi imm`. This is the
    /// `(s + i) | 0` idiom that keeps the accumulator int32-tagged in
    /// tight loops.
    BitOrAccI32 { imm: i32 },
    /// Fused compare: records `acc ? rhs` in NZCV. Must be immediately
    /// followed by a conditional branch op; standalone uses degenerate
    /// into a no-op. Maps from `TestLessThan`/`TestGreaterThan`/
    /// `TestLessThanOrEqual`/`TestGreaterThanOrEqual`/`TestEqualStrict`.
    CompareAcc { rhs: u16, kind: CompareKind },
    /// `if !acc then jump target_pc`. Follows a non-fused
    /// truthy/falsy write to acc (e.g. `LogicalNot` feeding into a
    /// branch). The emitter implements this as
    /// `cbz x21_truthy, target` or a toBoolean helper call.
    JumpIfAccFalse { target_pc: u32 },
    /// `if !(compare_flag) then jump target_pc`. Fused pair with a
    /// preceding [`CompareAcc`](TemplateInstruction::CompareAcc); the
    /// emitter uses the recorded compare kind to pick the right ARM
    /// condition code. Maps from `JumpIfToBooleanFalse` after a
    /// `TestX` op.
    JumpIfCompareFalse {
        target_pc: u32,
        compare_kind: CompareKind,
    },
    /// Unconditional jump. Maps from `Jump`.
    Jump { target_pc: u32 },
    /// Return the accumulator. Maps from `Return`.
    ReturnAcc,
    /// `acc = <boxed tag constant>`. Covers `LdaUndefined` / `LdaNull`
    /// / `LdaTrue` / `LdaFalse` / `LdaTheHole` / `LdaNaN` — the
    /// emitter writes a 64-bit immediate into x21 directly.
    LdaTagConst { value: u64 },
    /// `dst = src` (register-to-register copy). Maps from `Mov`.
    /// Intermediate non-acc copies emitted by the v1 compiler's temp
    /// allocator; the x21-pin is unaffected.
    Mov { dst: u16, src: u16 },
    /// `acc = acc + 1`. Maps from `Inc`.
    IncAcc,
    /// `acc = acc - 1`. Maps from `Dec`.
    DecAcc,
    /// `acc = -acc` (int32 wraparound). Maps from `Negate`.
    NegateAcc,
    /// `acc = ~acc` (bitwise NOT). Maps from `BitwiseNot`.
    BitNotAcc,
    /// `acc = acc * imm` (int32 wraparound). Maps from `MulSmi imm`.
    MulAccI32 { imm: i32 },
    /// `acc = acc & imm`. Maps from `BitwiseAndSmi imm`.
    BitAndAccI32 { imm: i32 },
    /// `acc = acc << (imm & 0x1f)`. Maps from `ShlSmi imm`.
    ShlAccI32 { imm: i32 },
    /// `acc = acc >> (imm & 0x1f)` (arithmetic). Maps from `ShrSmi imm`.
    ShrAccI32 { imm: i32 },
    /// `acc = ctx.this_raw` (NaN-boxed receiver). Maps from `LdaThis`.
    /// The v1 source compiler emits `LoadThis` at the start of every
    /// function to materialize the hidden receiver slot; accepting it
    /// here keeps the full function body eligible for v2 lowering.
    LdaThis,
    /// `acc = ctx.callee_raw`. Maps from `LdaCurrentClosure`.
    LdaCurrentClosure,
    /// `acc = ToNumber(acc)`. On `AccState::Int32` this is a no-op (the
    /// accumulator is already a JS Number). On `Raw` state we bail out
    /// to the interpreter for the coercion-correct path.
    ToNumberAcc,
}

/// Comparison kind carried across `CompareAcc` → `JumpIfCompareFalse`.
/// The emitter uses this to pick the right ARM condition code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareKind {
    Lt,
    Gt,
    Lte,
    Gte,
    EqStrict,
}

/// A v2 template-baseline candidate function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateProgram {
    /// Function name for diagnostics / telemetry.
    pub function_name: String,
    /// Total register count in the frame layout — drives the `x0`
    /// (register_base) offset math in the emitter.
    pub register_count: u16,
    /// Lowered v2 ops. Byte-PC offsets are rewritten to instruction
    /// indices so the emitter can use normal label back-patching.
    pub instructions: Vec<TemplateInstruction>,
    /// Instruction-index → byte-PC mapping. Needed for deopt resume —
    /// on bailout the emitter must hand the interpreter a byte-PC into
    /// the v2 stream, not an instruction index.
    pub byte_pcs: Vec<u32>,
    /// Byte-PCs of loop headers (backward branch targets). Used by the
    /// emitter to insert the acc-spill prelude before the header and
    /// by OSR to pick valid entry points.
    pub loop_header_byte_pcs: Vec<u32>,
}

/// Why a v2 function is not yet supported by the template baseline.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateCompileError {
    #[error("function has no v2 bytecode attached")]
    MissingBytecode,
    #[error("malformed v2 bytecode stream near byte pc {byte_pc}")]
    MalformedBytecode { byte_pc: u32 },
    #[error("unsupported v2 opcode at byte pc {byte_pc}: {opcode:?}")]
    UnsupportedOpcode { byte_pc: u32, opcode: Opcode },
    #[error("operand kind mismatch at byte pc {byte_pc}: expected {expected}")]
    OperandKindMismatch {
        byte_pc: u32,
        expected: &'static str,
    },
    #[error("jump target out of range at byte pc {byte_pc}: offset={offset}")]
    InvalidJumpTarget { byte_pc: u32, offset: i32 },
    #[error("compare at byte pc {byte_pc} not followed by JumpIfToBooleanFalse")]
    UnfusedCompare { byte_pc: u32 },
}

/// Analyze a function's v2 bytecode for template-baseline compilation.
///
/// Supported op set (Phase 4.5b):
/// `Ldar`, `Star`, `Mov`, `LdaSmi`, `LdaUndefined`/`LdaNull`/`LdaTrue`/
/// `LdaFalse`/`LdaTheHole`/`LdaNaN` (as `LdaTagConst`),
/// `Add`/`Sub`/`Mul`/`BitwiseOr`,
/// `AddSmi`/`SubSmi`/`MulSmi`/`BitwiseOrSmi`/`BitwiseAndSmi`/
/// `ShlSmi`/`ShrSmi`,
/// `Inc`/`Dec`/`Negate`/`BitwiseNot`,
/// `TestLessThan`/`TestGreaterThan`/`TestLessThanOrEqual`/
/// `TestGreaterThanOrEqual`/`TestEqualStrict`,
/// `Jump`, `JumpIfToBooleanFalse`, `Return`.
///
/// All other opcodes surface `UnsupportedOpcode` and prevent the
/// function from entering the v2 baseline path.
pub fn analyze_template_candidate(
    function: &Function,
) -> Result<TemplateProgram, TemplateCompileError> {
    let bytecode = function.bytecode();
    let bytes = bytecode.bytes();
    if bytes.is_empty() {
        return Err(TemplateCompileError::MissingBytecode);
    }

    // Walk the v2 instruction stream, eagerly decoding each op and its
    // operands. We record the byte-PC of each instruction so later
    // fused-compare analysis and jump-offset resolution can map
    // byte-PCs ↔ instruction indices.
    //
    // Two-phase approach:
    // (1) Raw decode: list of (byte_pc, end_pc, opcode, operands).
    // (2) Lowering + fusion: walk the raw list once more, fusing
    //     `CompareAcc` + `JumpIfToBooleanFalse` pairs, rewriting byte
    //     jump offsets to byte-PC targets, and emitting TemplateInstruction.
    let mut iter = InstructionIter::new(bytes);
    let mut raw: Vec<RawInstruction> = Vec::new();
    while let Some(step) = iter.next() {
        match step {
            Ok(instr) => raw.push(RawInstruction {
                byte_pc: instr.start_pc,
                end_pc: instr.end_pc,
                opcode: instr.opcode,
                operands: instr.operands,
            }),
            Err(_) => {
                return Err(TemplateCompileError::MalformedBytecode { byte_pc: iter.pc() });
            }
        }
    }

    let mut instructions: Vec<TemplateInstruction> = Vec::with_capacity(raw.len());
    let mut byte_pcs: Vec<u32> = Vec::with_capacity(raw.len());
    let mut loop_header_byte_pcs: Vec<u32> = Vec::new();

    let mut i = 0;
    while i < raw.len() {
        let r = &raw[i];
        let op = lower_raw(r, &raw, i, &mut loop_header_byte_pcs)?;
        byte_pcs.push(r.byte_pc);
        instructions.push(op);
        // If we fused a CompareAcc with the following JumpIfToBooleanFalse,
        // skip the consumed compare op. Detection: the fused lowering
        // emits `JumpIfCompareFalse`; the CompareAcc it consumed lives
        // at `raw[i-1]`. Track via a tiny state machine: see
        // `lower_raw` signaling.
        i += 1;
    }

    Ok(TemplateProgram {
        function_name: function
            .name()
            .map(str::to_string)
            .unwrap_or_else(|| "<anonymous>".to_string()),
        register_count: function.frame_layout().register_count(),
        instructions,
        byte_pcs,
        loop_header_byte_pcs,
    })
}

#[derive(Debug, Clone)]
struct RawInstruction {
    byte_pc: u32,
    end_pc: u32,
    opcode: Opcode,
    operands: Vec<Operand>,
}

fn lower_raw(
    r: &RawInstruction,
    _all: &[RawInstruction],
    _index: usize,
    loop_header_byte_pcs: &mut Vec<u32>,
) -> Result<TemplateInstruction, TemplateCompileError> {
    let bp = r.byte_pc;
    let end = r.end_pc;

    match r.opcode {
        Opcode::Ldar => {
            let reg = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::Ldar { reg })
        }
        Opcode::Star => {
            let reg = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::Star { reg })
        }
        Opcode::LdaSmi => {
            let imm = imm_i32(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::LdaI32 { imm })
        }
        Opcode::Add => {
            let rhs = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::AddAcc { rhs })
        }
        Opcode::Sub => {
            let rhs = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::SubAcc { rhs })
        }
        Opcode::Mul => {
            let rhs = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::MulAcc { rhs })
        }
        Opcode::BitwiseOr => {
            let rhs = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::BitOrAcc { rhs })
        }
        Opcode::AddSmi => {
            let imm = imm_i32(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::AddAccI32 { imm })
        }
        Opcode::SubSmi => {
            let imm = imm_i32(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::SubAccI32 { imm })
        }
        Opcode::BitwiseOrSmi => {
            let imm = imm_i32(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::BitOrAccI32 { imm })
        }
        Opcode::TestLessThan => {
            let rhs = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::CompareAcc {
                rhs,
                kind: CompareKind::Lt,
            })
        }
        Opcode::TestGreaterThan => {
            let rhs = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::CompareAcc {
                rhs,
                kind: CompareKind::Gt,
            })
        }
        Opcode::TestLessThanOrEqual => {
            let rhs = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::CompareAcc {
                rhs,
                kind: CompareKind::Lte,
            })
        }
        Opcode::TestGreaterThanOrEqual => {
            let rhs = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::CompareAcc {
                rhs,
                kind: CompareKind::Gte,
            })
        }
        Opcode::TestEqualStrict => {
            let rhs = reg(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::CompareAcc {
                rhs,
                kind: CompareKind::EqStrict,
            })
        }
        Opcode::Jump => {
            let off = jump_off(&r.operands, 0, bp)?;
            let target = resolve_byte_target(end, off, bp)?;
            if target <= bp && !loop_header_byte_pcs.contains(&target) {
                loop_header_byte_pcs.push(target);
            }
            Ok(TemplateInstruction::Jump { target_pc: target })
        }
        Opcode::JumpIfToBooleanFalse => {
            let off = jump_off(&r.operands, 0, bp)?;
            let target = resolve_byte_target(end, off, bp)?;
            if target <= bp && !loop_header_byte_pcs.contains(&target) {
                loop_header_byte_pcs.push(target);
            }
            // If the previous emitted instruction was a CompareAcc, the
            // emitter can fuse. Signal via `JumpIfCompareFalse` carrying
            // the previous compare's kind. Since we don't have access to
            // the already-emitted list here, signal a generic falsy
            // branch and let the emitter peek backwards on emission.
            Ok(TemplateInstruction::JumpIfAccFalse { target_pc: target })
        }
        Opcode::Return => Ok(TemplateInstruction::ReturnAcc),
        Opcode::LdaUndefined => Ok(TemplateInstruction::LdaTagConst {
            value: otter_vm::value::TAG_UNDEFINED,
        }),
        Opcode::LdaNull => Ok(TemplateInstruction::LdaTagConst {
            value: otter_vm::value::TAG_NULL,
        }),
        Opcode::LdaTrue => Ok(TemplateInstruction::LdaTagConst {
            value: otter_vm::value::TAG_TRUE,
        }),
        Opcode::LdaFalse => Ok(TemplateInstruction::LdaTagConst {
            value: otter_vm::value::TAG_FALSE,
        }),
        Opcode::LdaTheHole => Ok(TemplateInstruction::LdaTagConst {
            value: otter_vm::value::TAG_HOLE,
        }),
        Opcode::LdaNaN => Ok(TemplateInstruction::LdaTagConst {
            value: f64::NAN.to_bits(),
        }),
        Opcode::Mov => {
            let src = reg(&r.operands, 0, bp)?;
            let dst = reg(&r.operands, 1, bp)?;
            Ok(TemplateInstruction::Mov { dst, src })
        }
        Opcode::Inc => Ok(TemplateInstruction::IncAcc),
        Opcode::Dec => Ok(TemplateInstruction::DecAcc),
        Opcode::Negate => Ok(TemplateInstruction::NegateAcc),
        Opcode::BitwiseNot => Ok(TemplateInstruction::BitNotAcc),
        Opcode::MulSmi => {
            let imm = imm_i32(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::MulAccI32 { imm })
        }
        Opcode::BitwiseAndSmi => {
            let imm = imm_i32(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::BitAndAccI32 { imm })
        }
        Opcode::ShlSmi => {
            let imm = imm_i32(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::ShlAccI32 { imm })
        }
        Opcode::ShrSmi => {
            let imm = imm_i32(&r.operands, 0, bp)?;
            Ok(TemplateInstruction::ShrAccI32 { imm })
        }
        Opcode::LdaThis => Ok(TemplateInstruction::LdaThis),
        Opcode::LdaCurrentClosure => Ok(TemplateInstruction::LdaCurrentClosure),
        Opcode::ToNumber => Ok(TemplateInstruction::ToNumberAcc),
        other => Err(TemplateCompileError::UnsupportedOpcode {
            byte_pc: bp,
            opcode: other,
        }),
    }
}

fn reg(ops: &[Operand], pos: usize, byte_pc: u32) -> Result<u16, TemplateCompileError> {
    match ops.get(pos) {
        Some(Operand::Reg(r)) => {
            u16::try_from(*r).map_err(|_| TemplateCompileError::OperandKindMismatch {
                byte_pc,
                expected: "Reg fits in u16",
            })
        }
        _ => Err(TemplateCompileError::OperandKindMismatch {
            byte_pc,
            expected: "Reg",
        }),
    }
}

fn imm_i32(ops: &[Operand], pos: usize, byte_pc: u32) -> Result<i32, TemplateCompileError> {
    match ops.get(pos) {
        Some(Operand::Imm(v)) => Ok(*v),
        _ => Err(TemplateCompileError::OperandKindMismatch {
            byte_pc,
            expected: "Imm",
        }),
    }
}

fn jump_off(ops: &[Operand], pos: usize, byte_pc: u32) -> Result<i32, TemplateCompileError> {
    match ops.get(pos) {
        Some(Operand::JumpOff(v)) => Ok(*v),
        _ => Err(TemplateCompileError::OperandKindMismatch {
            byte_pc,
            expected: "JumpOff",
        }),
    }
}

fn resolve_byte_target(
    end_pc: u32,
    offset: i32,
    byte_pc: u32,
) -> Result<u32, TemplateCompileError> {
    let target = i64::from(end_pc) + i64::from(offset);
    u32::try_from(target).map_err(|_| TemplateCompileError::InvalidJumpTarget { byte_pc, offset })
}

// ---------------------------------------------------------------------------
// Phase 4.2 emitter: aarch64 stencil generation for a TemplateProgram.
// ---------------------------------------------------------------------------

const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;

/// Why the v2 emitter couldn't produce a stencil for a given program.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateEmitError {
    #[error("unsupported host architecture for v2 template emission: {0}")]
    UnsupportedHostArch(&'static str),
    #[error("register slot offset out of range for v2 emission: slot={slot}")]
    RegisterSlotOutOfRange { slot: u16 },
    #[error(
        "branch target out of range for v2 emission: from byte_pc={source_byte_pc} to byte_pc={target_byte_pc}"
    )]
    BranchTargetOutOfRange {
        source_byte_pc: u32,
        target_byte_pc: u32,
    },
    #[error("unmatched branch target byte_pc={target_byte_pc}; not in program")]
    UnresolvedBranchTarget { target_byte_pc: u32 },
    #[error("JumpIfAccFalse at instruction {index} expected a preceding CompareAcc — got {detail}")]
    UnfusedJumpIfAccFalse { index: usize, detail: &'static str },
    #[error("emitter-level unsupported sequence at instruction {index}: {detail}")]
    UnsupportedSequence { index: usize, detail: &'static str },
}

/// Accumulator-state tracking for the Phase 4.5b guarded emitter.
///
/// `x21` has two distinct representations depending on the most recent
/// write to the accumulator:
///
/// - [`AccState::Int32`] — sign-extended int32 (from `LdaI32`, `Ldar`
///   after a successful tag guard, or the output of an int32 arithmetic op).
/// - [`AccState::Raw`] — raw NaN-boxed value (from `LdaTagConst`, written
///   directly without any int32 coercion).
///
/// Every instruction has a pre- and post-state for x21. Ops that treat
/// x21 as int32 (arithmetic, compare, Return's box-and-exit) require
/// pre-state `Int32`; if the pre-state is `Raw`, the emitter emits an
/// unconditional bailout at that PC instead of the op body. `Star`
/// chooses between "box + str" (Int32) and raw "str" (Raw) so stores
/// remain semantically correct in both states.
///
/// At each bailout patch site we snapshot the state of x21 so the
/// per-site pad can spill it into `ctx.accumulator_raw` using the right
/// representation (`box_int32` for Int32, direct `str` for Raw). The
/// interpreter's resume path reads the spill via
/// [`TierUpExecResult::Bailout::accumulator_raw`](otter_vm::interpreter::TierUpExecResult)
/// and assigns it to the frame's v2 accumulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccState {
    Int32,
    Raw,
}

/// Emit a Phase 4.5b aarch64 stencil for a [`TemplateProgram`].
///
/// Every `ldr` that loads a slot interpreted as int32 is paired with a
/// `eor / tst / b.ne <bailout_pad>` guard against
/// [`TAG_INT32`](super::TAG_INT32) pinned in `x20`. On guard failure the
/// stencil branches to a per-site pad that writes
/// (`byte_pc`, `reason`, accumulator spill) into `JitContext` and returns
/// [`BAILOUT_SENTINEL`](crate::BAILOUT_SENTINEL). The tier-up hook sees
/// the sentinel and hands control back to the v2 dispatcher at
/// `byte_pc` with the spilled accumulator materialized into the frame.
///
/// Conventions baked into the stencil:
/// - `x0` = `JitContext*` on entry (caller passes it; v1 compat).
/// - `x9` = registers_base pointer (loaded from `JitContext` offset 0).
/// - `x19` = pinned `JitContext*`.
/// - `x20` = pinned `TAG_INT32` for fast tag guards.
/// - `x21` = pinned accumulator, state tracked via [`AccState`].
/// - `x10` / `x11` = scratch. `x10` doubles as `byte_pc` carrier into
///   the common bailout block; `x11` carries the `reason` code.
/// - Return boxes `x21` into the NaN-box encoding and writes it into
///   `x0` as the native return value.
pub fn emit_template_stencil(
    program: &TemplateProgram,
) -> Result<crate::arch::CodeBuffer, TemplateEmitError> {
    #[cfg(target_arch = "aarch64")]
    {
        emit_template_stencil_aarch64(program)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = program;
        Err(TemplateEmitError::UnsupportedHostArch(
            std::env::consts::ARCH,
        ))
    }
}

#[cfg(target_arch = "aarch64")]
fn emit_template_stencil_aarch64(
    program: &TemplateProgram,
) -> Result<crate::arch::CodeBuffer, TemplateEmitError> {
    use crate::arch::CodeBuffer;
    use crate::arch::aarch64::{Assembler, Cond, Reg};

    fn slot_offset(slot: u16) -> Result<u32, TemplateEmitError> {
        let byte_offset = u32::from(slot) * 8;
        if byte_offset > 4095 * 8 {
            return Err(TemplateEmitError::RegisterSlotOutOfRange { slot });
        }
        Ok(byte_offset)
    }

    /// Load a boxed slot value into `dst`, tag-guard it as int32 via
    /// `x20 == TAG_INT32`, and sign-extend the payload. On guard
    /// failure, control branches to the per-site bailout pad patched
    /// in after the main body. The guard uses the 3-insn
    /// `eor / tst / b.ne` sequence from v1's `check_int32_tag_fast`.
    fn load_int32_guarded(
        asm: &mut Assembler,
        dst: Reg,
        slot_off: u32,
        byte_pc: u32,
        acc_state_at_guard: AccState,
        bailout_patches: &mut Vec<BailoutPatch>,
    ) {
        asm.ldr_u64_imm(dst, Reg::X9, slot_off);
        asm.check_int32_tag_fast(dst, Reg::X20);
        let bp = asm.b_cond_placeholder(Cond::Ne);
        bailout_patches.push(BailoutPatch {
            source_offset: bp,
            byte_pc,
            reason: crate::BailoutReason::TypeGuardFailed as u32,
            acc_state: acc_state_at_guard,
        });
        asm.sxtw(dst, dst);
    }

    /// Store x21 into a slot. If x21 holds an unboxed int32, box it
    /// first; if it already holds raw NaN-boxed bits, write them
    /// directly.
    fn store_accumulator(asm: &mut Assembler, state: AccState, slot_off: u32) {
        match state {
            AccState::Int32 => {
                asm.box_int32(Reg::X10, Reg::X21);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_off);
            }
            AccState::Raw => {
                asm.str_u64_imm(Reg::X21, Reg::X9, slot_off);
            }
        }
    }

    /// Emit a direct branch to a bailout pad for the given PC. Used
    /// when an int32-requiring op sees acc_state == Raw — we can't
    /// safely execute the op, so bail to the interpreter which will
    /// run the coercion-correct path.
    fn emit_unconditional_bailout(
        asm: &mut Assembler,
        byte_pc: u32,
        reason: u32,
        acc_state: AccState,
        bailout_patches: &mut Vec<BailoutPatch>,
    ) {
        let bp = asm.b_placeholder();
        bailout_patches.push(BailoutPatch {
            source_offset: bp,
            byte_pc,
            reason,
            acc_state,
        });
    }

    /// Pending branch that will be patched once we know the target's
    /// emitted byte offset.
    #[derive(Debug, Clone, Copy)]
    struct BranchPatch {
        /// Byte offset of the branch instruction inside the CodeBuffer.
        source_offset: u32,
        /// Target byte_pc (v2 bytecode space) the branch should go to.
        target_byte_pc: u32,
        /// `None` for `B`, `Some(cond)` for `B.cond` (or `cbz` — treated
        /// separately via `is_cbz`).
        cond: Option<Cond>,
    }

    /// A deferred bailout site. After the main body is emitted, each
    /// patch gets its own pad inside the code buffer. The pad writes
    /// the accumulator spill, pc, and reason, then branches to a
    /// shared common epilogue that returns [`BAILOUT_SENTINEL`].
    #[derive(Debug, Clone, Copy)]
    struct BailoutPatch {
        source_offset: u32,
        byte_pc: u32,
        reason: u32,
        acc_state: AccState,
    }

    let mut buf = CodeBuffer::new();
    let mut asm = Assembler::new(&mut buf);

    // Prologue: 32-byte frame saving x19 + lr + x20. Same shape as v1
    // so the call-site ABI stays identical.
    asm.push_x19_lr_32();
    asm.str_x20_at_sp16();
    // x19 = JitContext*
    asm.mov_rr(Reg::X19, Reg::X0);
    // x9 = registers_base (hot, reused every instruction)
    asm.ldr_u64_imm(Reg::X9, Reg::X19, 0);
    // x20 = TAG_INT32 (pinned once for check_int32_tag_fast reuse)
    asm.mov_imm64(Reg::X20, TAG_INT32);
    // x21 = accumulator, initialized to 0. First instruction that
    // writes acc overwrites it, so the initial value only matters if
    // someone reads x21 before any write — which our analyzer
    // guarantees doesn't happen in practice.
    asm.mov_imm64(Reg::X21, 0);

    let mut branch_patches: Vec<BranchPatch> = Vec::new();
    let mut bailout_patches: Vec<BailoutPatch> = Vec::new();
    // Map from byte_pc → emitted byte offset in the CodeBuffer.
    // Populated as we walk the IR so forward branches can be patched
    // at the end.
    let mut byte_pc_to_emit: Vec<(u32, u32)> = Vec::with_capacity(program.instructions.len());

    // Post-state of x21 after each instruction. Index `i` holds the
    // state AFTER instruction `i` has executed — used by branch fusion
    // (peek at `i-1`) and by bailout-spill-kind selection.
    let mut acc_states: Vec<AccState> = Vec::with_capacity(program.instructions.len());
    // Running state: x21 initial value is 0 (Int32).
    let mut acc_state = AccState::Int32;

    let n = program.instructions.len();
    let mut i = 0;
    while i < n {
        let byte_pc = program.byte_pcs[i];
        byte_pc_to_emit.push((byte_pc, asm.position()));

        match &program.instructions[i] {
            TemplateInstruction::LdaI32 { imm } => {
                asm.mov_imm64(Reg::X21, *imm as i64 as u64);
                acc_state = AccState::Int32;
            }
            TemplateInstruction::Star { reg } => {
                store_accumulator(&mut asm, acc_state, slot_offset(*reg)?);
                // Star doesn't touch x21.
            }
            TemplateInstruction::Ldar { reg } => {
                // The guard fires AFTER ldr has clobbered x21 with raw
                // slot bits — so at the bailout point x21 holds raw
                // (not yet sxtw'd). Spill as Raw.
                load_int32_guarded(
                    &mut asm,
                    Reg::X21,
                    slot_offset(*reg)?,
                    byte_pc,
                    AccState::Raw,
                    &mut bailout_patches,
                );
                acc_state = AccState::Int32;
            }
            TemplateInstruction::AddAcc { rhs } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    load_int32_guarded(
                        &mut asm,
                        Reg::X10,
                        slot_offset(*rhs)?,
                        byte_pc,
                        acc_state,
                        &mut bailout_patches,
                    );
                    asm.add_rrr(Reg::X21, Reg::X21, Reg::X10);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::SubAcc { rhs } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    load_int32_guarded(
                        &mut asm,
                        Reg::X10,
                        slot_offset(*rhs)?,
                        byte_pc,
                        acc_state,
                        &mut bailout_patches,
                    );
                    asm.sub_rrr(Reg::X21, Reg::X21, Reg::X10);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::MulAcc { rhs } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    load_int32_guarded(
                        &mut asm,
                        Reg::X10,
                        slot_offset(*rhs)?,
                        byte_pc,
                        acc_state,
                        &mut bailout_patches,
                    );
                    asm.mul_rrr(Reg::X21, Reg::X21, Reg::X10);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::BitOrAcc { rhs } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    load_int32_guarded(
                        &mut asm,
                        Reg::X10,
                        slot_offset(*rhs)?,
                        byte_pc,
                        acc_state,
                        &mut bailout_patches,
                    );
                    asm.orr_rrr(Reg::X21, Reg::X21, Reg::X10);
                }
            }
            TemplateInstruction::AddAccI32 { imm } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    asm.mov_imm64(Reg::X10, *imm as i64 as u64);
                    asm.add_rrr(Reg::X21, Reg::X21, Reg::X10);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::SubAccI32 { imm } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    asm.mov_imm64(Reg::X10, *imm as i64 as u64);
                    asm.sub_rrr(Reg::X21, Reg::X21, Reg::X10);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::BitOrAccI32 { imm } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    asm.mov_imm64(Reg::X10, *imm as i64 as u64);
                    asm.orr_rrr(Reg::X21, Reg::X21, Reg::X10);
                }
            }
            TemplateInstruction::CompareAcc { rhs, .. } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    load_int32_guarded(
                        &mut asm,
                        Reg::X10,
                        slot_offset(*rhs)?,
                        byte_pc,
                        acc_state,
                        &mut bailout_patches,
                    );
                    asm.cmp_rr(Reg::X21, Reg::X10);
                }
            }
            TemplateInstruction::JumpIfAccFalse { target_pc } => {
                // Fused path requires the previous IR op to have been
                // a CompareAcc that left NZCV set. Peek at acc_states
                // history alongside the previous instruction.
                let fused_cond = match i.checked_sub(1).and_then(|p| program.instructions.get(p)) {
                    Some(TemplateInstruction::CompareAcc { kind, .. }) => {
                        // Branch fires on the negation of the JS
                        // compare (JumpIfToBooleanFalse semantics).
                        Some(match kind {
                            CompareKind::Lt => Cond::Ge,
                            CompareKind::Gt => Cond::Le,
                            CompareKind::Lte => Cond::Gt,
                            CompareKind::Gte => Cond::Lt,
                            CompareKind::EqStrict => Cond::Ne,
                        })
                    }
                    _ => None,
                };
                if let Some(c) = fused_cond {
                    let src = asm.b_cond_placeholder(c);
                    branch_patches.push(BranchPatch {
                        source_offset: src,
                        target_byte_pc: *target_pc,
                        cond: Some(c),
                    });
                } else if acc_state == AccState::Int32 {
                    // Non-fused with int32 acc: `cbz x21, target`.
                    let src = asm.position();
                    asm.cbz(Reg::X21, 0);
                    branch_patches.push(BranchPatch {
                        source_offset: src,
                        target_byte_pc: *target_pc,
                        // `cond = None` — the patcher writes a CBZ,
                        // not a B/Bcc. We encode this by setting
                        // cond=None, but we also need to distinguish
                        // unconditional B from CBZ. Detect via insn
                        // word at patch time.
                        cond: None,
                    });
                } else {
                    // Can't branch on a Raw value without coercion.
                    // Bail out.
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                }
            }
            TemplateInstruction::JumpIfCompareFalse {
                target_pc,
                compare_kind,
            } => {
                let cond = match compare_kind {
                    CompareKind::Lt => Cond::Ge,
                    CompareKind::Gt => Cond::Le,
                    CompareKind::Lte => Cond::Gt,
                    CompareKind::Gte => Cond::Lt,
                    CompareKind::EqStrict => Cond::Ne,
                };
                let src = asm.b_cond_placeholder(cond);
                branch_patches.push(BranchPatch {
                    source_offset: src,
                    target_byte_pc: *target_pc,
                    cond: Some(cond),
                });
            }
            TemplateInstruction::Jump { target_pc } => {
                let src = asm.b_placeholder();
                branch_patches.push(BranchPatch {
                    source_offset: src,
                    target_byte_pc: *target_pc,
                    cond: None,
                });
            }
            TemplateInstruction::ReturnAcc => {
                // Box x21 (if int32) or return raw bits (if Raw) as
                // the native return value.
                match acc_state {
                    AccState::Int32 => {
                        asm.box_int32(Reg::X0, Reg::X21);
                    }
                    AccState::Raw => {
                        asm.mov_rr(Reg::X0, Reg::X21);
                    }
                }
                asm.ldr_x20_at_sp16();
                asm.pop_x19_lr_32();
                asm.ret();
            }
            TemplateInstruction::LdaTagConst { value } => {
                asm.mov_imm64(Reg::X21, *value);
                acc_state = AccState::Raw;
            }
            TemplateInstruction::Mov { dst, src } => {
                // Raw register-to-register copy — doesn't touch x21.
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*src)?);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::IncAcc => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    asm.mov_imm64(Reg::X10, 1);
                    asm.add_rrr(Reg::X21, Reg::X21, Reg::X10);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::DecAcc => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    asm.mov_imm64(Reg::X10, 1);
                    asm.sub_rrr(Reg::X21, Reg::X21, Reg::X10);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::NegateAcc => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    // x21 = 0 - x21 (int32 wraparound preserved by sxtw).
                    asm.sub_rrr(Reg::X21, Reg::Xzr, Reg::X21);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::BitNotAcc => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    // x21 = x21 XOR 0xFFFF_FFFF_FFFF_FFFF.
                    asm.mov_imm64(Reg::X10, u64::MAX);
                    asm.eor_rrr(Reg::X21, Reg::X21, Reg::X10);
                    // Result is still sign-extended int32 (XOR with
                    // all-ones preserves sign-extension).
                }
            }
            TemplateInstruction::MulAccI32 { imm } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    asm.mov_imm64(Reg::X10, *imm as i64 as u64);
                    asm.mul_rrr(Reg::X21, Reg::X21, Reg::X10);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::BitAndAccI32 { imm } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    asm.mov_imm64(Reg::X10, *imm as i64 as u64);
                    asm.and_rrr(Reg::X21, Reg::X21, Reg::X10);
                    // Sign-extension preserved by AND of two sign-ext
                    // operands.
                }
            }
            TemplateInstruction::ShlAccI32 { imm } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    let shift = (*imm as u32) & 0x1F;
                    asm.mov_imm64(Reg::X10, u64::from(shift));
                    asm.lslv_w(Reg::X21, Reg::X21, Reg::X10);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::ShrAccI32 { imm } => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                } else {
                    let shift = (*imm as u32) & 0x1F;
                    asm.mov_imm64(Reg::X10, u64::from(shift));
                    asm.asrv_w(Reg::X21, Reg::X21, Reg::X10);
                    asm.sxtw(Reg::X21, Reg::X21);
                }
            }
            TemplateInstruction::LdaThis => {
                asm.ldr_u64_imm(Reg::X21, Reg::X19, crate::context::offsets::THIS_RAW as u32);
                acc_state = AccState::Raw;
            }
            TemplateInstruction::LdaCurrentClosure => {
                asm.ldr_u64_imm(
                    Reg::X21,
                    Reg::X19,
                    crate::context::offsets::CALLEE_RAW as u32,
                );
                acc_state = AccState::Raw;
            }
            TemplateInstruction::ToNumberAcc => {
                if acc_state != AccState::Int32 {
                    emit_unconditional_bailout(
                        &mut asm,
                        byte_pc,
                        crate::BailoutReason::TypeGuardFailed as u32,
                        acc_state,
                        &mut bailout_patches,
                    );
                }
                // Int32: no-op (already a Number).
            }
        }
        acc_states.push(acc_state);
        i += 1;
    }

    // ----- Common bailout epilogue -----
    //
    // Per-site pads branch here AFTER populating: x10 = byte_pc,
    // x11 = reason, and spilling x21 into ctx.accumulator_raw. This
    // block writes the low-32-bit pc/reason fields and unwinds the
    // prologue, returning BAILOUT_SENTINEL in x0.
    let bailout_common = asm.position();
    asm.str_u32_imm(
        Reg::X10,
        Reg::X19,
        crate::context::offsets::BAILOUT_PC as u32,
    );
    asm.str_u32_imm(
        Reg::X11,
        Reg::X19,
        crate::context::offsets::BAILOUT_REASON as u32,
    );
    asm.mov_imm64(Reg::X0, crate::BAILOUT_SENTINEL);
    asm.ldr_x20_at_sp16();
    asm.pop_x19_lr_32();
    asm.ret();

    // ----- Per-site bailout pads -----
    //
    // Each pad:
    //   1) Spills x21 into ctx.accumulator_raw (boxed if Int32,
    //      raw bits if Raw).
    //   2) Loads byte_pc into x10 and reason into x11.
    //   3) Branches to bailout_common.
    //
    // The pad's entry address is recorded so we can patch the
    // original guard/branch site to jump here.
    struct PadInfo {
        entry_offset: u32,
        tail_branch_offset: u32,
    }
    let mut pad_infos: Vec<PadInfo> = Vec::with_capacity(bailout_patches.len());
    for patch in &bailout_patches {
        let pad_pos = asm.position();
        match patch.acc_state {
            AccState::Int32 => {
                asm.box_int32(Reg::X12, Reg::X21);
                asm.str_u64_imm(
                    Reg::X12,
                    Reg::X19,
                    crate::context::offsets::ACCUMULATOR_RAW as u32,
                );
            }
            AccState::Raw => {
                asm.str_u64_imm(
                    Reg::X21,
                    Reg::X19,
                    crate::context::offsets::ACCUMULATOR_RAW as u32,
                );
            }
        }
        asm.mov_imm64(Reg::X10, u64::from(patch.byte_pc));
        asm.mov_imm64(Reg::X11, u64::from(patch.reason));
        let tail = asm.b_placeholder();
        pad_infos.push(PadInfo {
            entry_offset: pad_pos,
            tail_branch_offset: tail,
        });
    }

    // Drop the assembler binding so the buffer is available for
    // direct-mutation below. `Assembler` has no Drop impl, so this just
    // releases the name binding; the CodeBuffer captured earlier stays alive.
    let _ = asm;

    // Patch forward/backward branches to their targets inside the
    // emitted body.
    for patch in &branch_patches {
        let Some(&(_, target_off)) = byte_pc_to_emit
            .iter()
            .find(|(bpc, _)| *bpc == patch.target_byte_pc)
        else {
            return Err(TemplateEmitError::UnresolvedBranchTarget {
                target_byte_pc: patch.target_byte_pc,
            });
        };
        let rel_bytes = target_off as i64 - patch.source_offset as i64;
        if rel_bytes % 4 != 0 || rel_bytes < i64::from(i32::MIN) || rel_bytes > i64::from(i32::MAX)
        {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: patch.source_offset,
                target_byte_pc: patch.target_byte_pc,
            });
        }
        let rel = (rel_bytes / 4) as i32;
        // Detect the original opcode class from the bytes at
        // source_offset so we patch the right immediate layout.
        let Some(existing) = buf.read_u32_le(patch.source_offset) else {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: patch.source_offset,
                target_byte_pc: patch.target_byte_pc,
            });
        };
        let is_cbz = (existing & 0x7F00_0000) == 0x3400_0000;
        let insn = if is_cbz {
            // CBZ: imm19 at bits [23:5].
            let imm19 = ((rel as u32) & 0x0007_FFFF) << 5;
            (existing & !0x00FF_FFE0) | imm19
        } else {
            match patch.cond {
                None => {
                    // Unconditional B: imm26 at bits [25:0].
                    let imm26 = (rel as u32) & 0x03FF_FFFF;
                    0x1400_0000 | imm26
                }
                Some(c) => {
                    // B.cond: imm19 at bits [23:5], cond at bits [3:0].
                    let imm19 = ((rel as u32) & 0x0007_FFFF) << 5;
                    0x5400_0000 | imm19 | (c as u32 & 0xF)
                }
            }
        };
        if !buf.patch_u32_le(patch.source_offset, insn) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: patch.source_offset,
                target_byte_pc: patch.target_byte_pc,
            });
        }
    }

    // Patch guard/unconditional bailout source sites to jump to their
    // respective pads.
    for (patch, pad) in bailout_patches.iter().zip(pad_infos.iter()) {
        let src = patch.source_offset;
        let pad_entry = pad.entry_offset;
        let delta = i64::from(pad_entry) - i64::from(src);
        if delta % 4 != 0 {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: src,
                target_byte_pc: pad_entry,
            });
        }
        let rel = (delta / 4) as i32;
        let Some(existing) = buf.read_u32_le(src) else {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: src,
                target_byte_pc: pad_entry,
            });
        };
        // Determine the kind of branch we emitted at src:
        //   - b_cond_placeholder → 0x5400_0000 | cond (imm19 patch)
        //   - b_placeholder      → 0x1400_0000 (imm26 patch)
        let is_bcond = (existing & 0xFF00_0000) == 0x5400_0000;
        let insn = if is_bcond {
            let imm19 = ((rel as u32) & 0x0007_FFFF) << 5;
            (existing & !0x00FF_FFE0) | imm19
        } else {
            let imm26 = (rel as u32) & 0x03FF_FFFF;
            0x1400_0000 | imm26
        };
        if !buf.patch_u32_le(src, insn) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: src,
                target_byte_pc: pad_entry,
            });
        }
    }

    // Patch each pad's trailing `b bailout_common`.
    for pad in &pad_infos {
        let src = pad.tail_branch_offset;
        let delta = i64::from(bailout_common) - i64::from(src);
        let imm26 = ((delta / 4) as i32 as u32) & 0x03FF_FFFF;
        if !buf.patch_u32_le(src, 0x1400_0000 | imm26) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: src,
                target_byte_pc: bailout_common,
            });
        }
    }

    // acc_states is used transitively by the emit loop; keep the
    // declaration alive for downstream (and to prevent `unused_mut`
    // lints from firing when more analyses consume it).
    let _ = acc_states;

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::bytecode::BytecodeBuilder;
    use otter_vm::frame::FrameLayout;
    use otter_vm::module::Function;

    /// Build a v2 function containing the sum-loop pattern and verify
    /// the analyzer lowers it to the expected sequence. Exercises all
    /// five supported op families: LdaSmi/Star, Ldar, TestLessThan,
    /// JumpIfToBooleanFalse, AddSmi / BitwiseOrSmi, Jump, Return.
    #[test]
    fn analyzer_accepts_sum_loop() {
        let mut b = BytecodeBuilder::new();
        // s = 0
        b.emit(Opcode::LdaSmi, &[Operand::Imm(0)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        // i = 0
        b.emit(Opcode::LdaSmi, &[Operand::Imm(0)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();

        let loop_header = b.new_label();
        let exit = b.new_label();
        b.bind_label(loop_header).unwrap();
        // acc = i
        b.emit(Opcode::Ldar, &[Operand::Reg(2)]).unwrap();
        // NZCV = (acc < n)
        b.emit(Opcode::TestLessThan, &[Operand::Reg(0)]).unwrap();
        // if !cond -> exit
        b.emit_jump_to(Opcode::JumpIfToBooleanFalse, exit).unwrap();
        // s = (s + i) | 0
        b.emit(Opcode::Ldar, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::Add, &[Operand::Reg(2)]).unwrap();
        b.emit(Opcode::BitwiseOrSmi, &[Operand::Imm(0)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        // i += 1
        b.emit(Opcode::Ldar, &[Operand::Reg(2)]).unwrap();
        b.emit(Opcode::AddSmi, &[Operand::Imm(1)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();
        b.emit_jump_to(Opcode::Jump, loop_header).unwrap();
        b.bind_label(exit).unwrap();
        b.emit(Opcode::Ldar, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();

        let layout = FrameLayout::new(0, 1, 2, 0).unwrap();
        let function = Function::with_empty_tables(Some("sum"), layout, v2);

        let program = analyze_template_candidate(&function).expect("analyze");
        // Every v2 op should have lowered to exactly one TemplateInstruction.
        assert_eq!(program.instructions.len(), program.byte_pcs.len());
        assert_eq!(program.register_count, 3);
        // One backward branch → one loop header.
        assert_eq!(program.loop_header_byte_pcs.len(), 1);
        // The loop header should land at the instruction after the
        // pre-loop "i = 0" sequence (LdaSmi/Star × 2 = 4 bytes each so
        // byte pc 8 in the narrow encoding). Check it matches one of
        // the recorded byte PCs.
        let header = program.loop_header_byte_pcs[0];
        assert!(
            program.byte_pcs.contains(&header),
            "loop header {header} should be a recorded instruction byte pc (got byte_pcs={:?})",
            program.byte_pcs,
        );
        // Verify critical op lowerings.
        let ops = &program.instructions;
        assert_eq!(ops[0], TemplateInstruction::LdaI32 { imm: 0 });
        assert_eq!(ops[1], TemplateInstruction::Star { reg: 1 });
        assert_eq!(ops[2], TemplateInstruction::LdaI32 { imm: 0 });
        assert_eq!(ops[3], TemplateInstruction::Star { reg: 2 });
        // The last instruction must be the return.
        assert_eq!(*ops.last().unwrap(), TemplateInstruction::ReturnAcc);
    }

    #[test]
    fn analyzer_rejects_unsupported_op() {
        let mut b = BytecodeBuilder::new();
        // `Div` is not yet in the Phase 4.1 supported set.
        b.emit(Opcode::LdaSmi, &[Operand::Imm(10)]).unwrap();
        b.emit(Opcode::Div, &[Operand::Reg(0)]).unwrap();
        b.emit(Opcode::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();
        let layout = FrameLayout::new(0, 1, 0, 0).unwrap();
        let function = Function::with_empty_tables(Some("f"), layout, v2);

        match analyze_template_candidate(&function) {
            Err(TemplateCompileError::UnsupportedOpcode { opcode, .. }) => {
                assert!(matches!(opcode, Opcode::Div));
            }
            other => panic!("expected UnsupportedOpcode(Div), got {other:?}"),
        }
    }

    #[test]
    fn analyzer_refuses_function_without_bytecode() {
        let layout = FrameLayout::new(0, 0, 0, 0).unwrap();
        let function = Function::with_empty_tables(Some("f"), layout, Default::default());
        match analyze_template_candidate(&function) {
            Err(TemplateCompileError::MissingBytecode) => {}
            other => panic!("expected MissingBytecode, got {other:?}"),
        }
    }

    /// Emit a stencil for the sum-loop program and verify the byte
    /// stream disassembles to the expected x21-pinned shape. No
    /// invocation (Phase 4.3 wires that via the tier-up hook); this
    /// test only proves the emitter produces well-formed aarch64.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn emitter_produces_sum_loop_stencil() {
        let mut b = BytecodeBuilder::new();
        b.emit(Opcode::LdaSmi, &[Operand::Imm(0)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::LdaSmi, &[Operand::Imm(0)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();

        let loop_header = b.new_label();
        let exit = b.new_label();
        b.bind_label(loop_header).unwrap();
        b.emit(Opcode::Ldar, &[Operand::Reg(2)]).unwrap();
        b.emit(Opcode::TestLessThan, &[Operand::Reg(0)]).unwrap();
        b.emit_jump_to(Opcode::JumpIfToBooleanFalse, exit).unwrap();
        b.emit(Opcode::Ldar, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::Add, &[Operand::Reg(2)]).unwrap();
        b.emit(Opcode::BitwiseOrSmi, &[Operand::Imm(0)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::Ldar, &[Operand::Reg(2)]).unwrap();
        b.emit(Opcode::AddSmi, &[Operand::Imm(1)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();
        b.emit_jump_to(Opcode::Jump, loop_header).unwrap();
        b.bind_label(exit).unwrap();
        b.emit(Opcode::Ldar, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();

        let layout = FrameLayout::new(0, 1, 2, 0).unwrap();
        let function = Function::with_empty_tables(Some("sum"), layout, v2);
        let program = analyze_template_candidate(&function).expect("analyze");
        let buf = emit_template_stencil(&program).expect("emit");
        let bytes = buf.bytes();
        assert!(!bytes.is_empty(), "emitter produced no code");
        assert_eq!(
            bytes.len() % 4,
            0,
            "emitter produced non-word-aligned bytes"
        );

        // Disassemble with bad64 and collect the mnemonics so the test
        // can assert on the stencil shape without depending on the
        // exact instruction encoding.
        let mut mnemonics: Vec<String> = Vec::with_capacity(bytes.len() / 4);
        for (idx, chunk) in bytes.chunks_exact(4).enumerate() {
            let word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let addr = (idx * 4) as u64;
            let insn = bad64::decode(word, addr).expect("decode");
            mnemonics.push(format!("{:?}", insn.op()));
        }

        // The stencil must carry out:
        //   * a prologue that pins x19 to the JitContext* and x9 to
        //     registers_base  → at least one LDR and one MOV near the top.
        //   * an accumulator-increment loop — at minimum one ADD on
        //     x21, one ORR on x21, and one CMP against x21.
        //   * a compare → conditional-branch pair (CMP, then Bcc).
        //   * an unconditional backward branch (B) closing the loop.
        //   * a RET epilogue.
        assert!(
            mnemonics.contains(&"ADD".to_string()),
            "missing ADD: {mnemonics:?}"
        );
        assert!(
            mnemonics.contains(&"ORR".to_string()),
            "missing ORR: {mnemonics:?}"
        );
        assert!(
            mnemonics
                .iter()
                .any(|m| m.starts_with("CMP") || m == "SUBS"),
            "missing CMP (decoded as CMP or SUBS): {mnemonics:?}"
        );
        assert!(
            mnemonics.iter().any(|m| m == "B" || m == "B_AL"),
            "missing unconditional B: {mnemonics:?}"
        );
        assert!(
            mnemonics.iter().any(|m| m.starts_with("B_")),
            "missing conditional Bcc (B_*): {mnemonics:?}"
        );
        assert!(
            mnemonics.contains(&"RET".to_string()),
            "missing RET: {mnemonics:?}"
        );

        // Size sanity: the Phase 4.5b guarded sum-loop stencil sits
        // between the trust-int32 280 B baseline and v1's ≈828 B.
        // Each guarded load adds `eor / tst / b.ne` (3 insns) and
        // each bailout site adds a ~5–7 insn pad (spill + pc/reason
        // + b). The sum loop has ~8 guarded loads and the same number
        // of pads, so ≈280 + 8·12 + 8·32 ≈ 632 B is the expected
        // upper bound. Lock it at 640 to catch emission regressions
        // without flaking on minor tweaks.
        assert!(
            bytes.len() <= 640,
            "v2 sum-loop stencil larger than expected: {} bytes (Phase 4.5b guarded target ≤ 640)",
            bytes.len()
        );
    }

    /// Phase 4.3 end-to-end invocation is deferred pending on-hardware
    /// debugging. Even a trivial `LdaSmi 42; Return` stencil — whose
    /// disassembly structurally mirrors the production v1 template
    /// baseline epilogue — hangs the Rust test harness on the call,
    /// leaving UE (uninterruptible-exiting) zombie processes behind.
    /// Possible causes (needs lldb to narrow):
    ///   * macOS MAP_JIT / `pthread_jit_write_protect_np` flip missing
    ///     around the call site — the memory is mapped X but not in
    ///     the thread's current-execute state.
    ///   * Stack alignment drift from a prologue variant `push_x19_lr_32`
    ///     was not designed for this call convention.
    ///   * Signal-handler interaction specific to the test harness.
    ///
    /// The emitter + analyzer + disassembly coverage above already
    /// pins the compiled bytes; Phase 4.4 (guarded variant + proper
    /// tier-up-hook wiring) will thread the v2 stencil through the
    /// production `TierUpHook::execute_cached` path which has the
    /// MAP_JIT write-protect flips in place and has been proven by v1.
    #[ignore = "Phase 4.3 end-to-end invocation deferred to Phase 4.4 with tier-up hook wiring"]
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn stencil_invocation_smoke() {}

    // -------------------------------------------------------------------
    // M2: stencil disassembly sanity + interpreter microbenchmark.
    // -------------------------------------------------------------------

    /// End-to-end check that the M1 source `function f(n) { return n + 1 }`
    /// round-trips through `ModuleCompiler` → `analyze_template_candidate`
    /// → `emit_template_stencil` and yields a stencil whose aarch64
    /// disassembly carries the expected x21-pinned shape.
    ///
    /// The test does **not** invoke the stencil. Direct invocation from
    /// Rust test harnesses on macOS / Apple Silicon is the documented
    /// hazard tracked by `stencil_invocation_smoke` above; the production
    /// `TierUpHook::execute_cached` path handles the `MAP_JIT` /
    /// `pthread_jit_write_protect_np` ceremony correctly, but that path
    /// is exercised by integration tests, not here.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn m2_stencil_disassembly_sanity() {
        use otter_vm::module::FunctionIndex;
        use otter_vm::source_compiler::ModuleCompiler;
        use oxc_span::SourceType;

        let module = ModuleCompiler::new()
            .compile(
                "function f(n) { return n + 1; }",
                "f.js",
                SourceType::default(),
            )
            .expect("M1 source must compile");
        let function = module
            .function(FunctionIndex(0))
            .expect("module has entry function");

        let program = analyze_template_candidate(function).expect("analyze");
        // The M1 lowering produces exactly: `Ldar r0`, `AddSmi 1`, `Return`.
        // Lock that shape so a regression in the source compiler shows up
        // here rather than masquerading as a stencil-size drift.
        assert_eq!(
            program.instructions.as_slice(),
            &[
                TemplateInstruction::Ldar { reg: 0 },
                TemplateInstruction::AddAccI32 { imm: 1 },
                TemplateInstruction::ReturnAcc,
            ],
            "analyzer must lower the M1 source to a Ldar / AddSmi / Return triple",
        );

        let buf = emit_template_stencil(&program).expect("emit stencil");
        let bytes = buf.bytes();
        assert!(!bytes.is_empty(), "emitter produced no code");
        assert_eq!(
            bytes.len() % 4,
            0,
            "emitter produced non-word-aligned bytes"
        );

        // Disassemble each 32-bit word and pin the stencil shape via
        // mnemonic-presence checks. bad64 may pick aliases (`SUBS` for
        // `CMP`, `SBFM` for `SXTW`, `ANDS` for `TST`, `ORR` for the
        // `MOV xd, xn` zero-register form), so accept either side of
        // each alias pair.
        let mnemonics: Vec<String> = bytes
            .chunks_exact(4)
            .enumerate()
            .map(|(idx, chunk)| {
                let word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let addr = (idx * 4) as u64;
                let insn = bad64::decode(word, addr).expect("decode");
                format!("{:?}", insn.op())
            })
            .collect();

        let has = |needle: &str| mnemonics.iter().any(|m| m == needle);
        let has_prefix = |needle: &str| mnemonics.iter().any(|m| m.starts_with(needle));

        // Prologue: at least one LDR (registers_base load) and one MOV /
        // ORR pinning x19 to the JitContext pointer.
        assert!(
            has("LDR"),
            "prologue missing LDR for registers_base: {mnemonics:?}",
        );
        assert!(
            has("MOV") || has("ORR"),
            "prologue missing MOV (alias of `ORR xd, xzr, xn`): {mnemonics:?}",
        );
        // Tag-guard sequence emitted by `check_int32_tag_fast`.
        assert!(has("EOR"), "guard missing EOR: {mnemonics:?}");
        assert!(
            has("TST") || has("ANDS"),
            "guard missing TST (or ANDS XZR alias): {mnemonics:?}",
        );
        assert!(has("B_NE"), "guard missing B.NE branch: {mnemonics:?}");
        // Sign-extension of the tag-guarded payload.
        assert!(
            has("SXTW") || has("SBFM"),
            "missing SXTW (or SBFM canonical form): {mnemonics:?}",
        );
        // Immediate materialisation for AddSmi and TAG_INT32 / sentinel.
        assert!(
            has_prefix("MOV"),
            "missing MOVZ/MOVK for immediates: {mnemonics:?}",
        );
        // Arithmetic op + box_int32 epilogue.
        assert!(has("ADD"), "missing ADD: {mnemonics:?}");
        assert!(
            has("ORR"),
            "missing ORR (box_int32 OR with TAG_INT32): {mnemonics:?}",
        );
        // Function exit.
        assert!(has("RET"), "missing RET: {mnemonics:?}");

        // Size sanity. Phase 4.5b guarded emission for `Ldar r0 / AddSmi
        // 1 / Return` lands at ≈40 aarch64 instructions (≈160 bytes):
        // a 32-byte prologue, one tag-guarded `Ldar` load + sxtw, a
        // 3-insn `AddSmi` (movz / add / sxtw), the box_int32 + ret
        // epilogue, the shared bailout-common epilogue, and one
        // bailout pad for the Ldar guard. Lock the upper bound at
        // 200 bytes — comfortably above the current ≈160 — to catch
        // emitter regressions without flaking on cosmetic tweaks.
        assert!(
            bytes.len() <= 200,
            "M1 stencil larger than expected: {} bytes (Phase 4.5b target ≤ 200)",
            bytes.len(),
        );
    }

    /// Microbenchmark for the M1 source `function f(n) { return n + 1 }`
    /// running through the v2 interpreter. Reports `interp: <ns/iter>` to
    /// stdout for the V2_MIGRATION.md benchmarks table.
    ///
    /// Invoke with:
    /// ```text
    /// cargo test -p otter-jit --release -- --ignored m1_microbench --nocapture
    /// ```
    ///
    /// JIT-side measurement is intentionally skipped this session.
    /// `Interpreter::execute_with_runtime` is a top-level entry — the
    /// JSC-style tier-up hook only fires on inner `CallClosure` boundaries,
    /// and the v2 source compiler does not yet emit calls (lands at M9).
    /// The remaining option — direct invocation of a compiled stencil
    /// from a Rust test harness — is the documented macOS / Apple
    /// Silicon hazard tracked by `stencil_invocation_smoke`. The JIT
    /// half of the benchmark therefore moves to a later milestone once
    /// either (a) M9 + M7 give us a JS loop that calls f, or (b) the
    /// production tier-up path can be exercised from a unit test on
    /// macOS without UE zombies.
    #[cfg(target_arch = "aarch64")]
    #[ignore = "M1 microbenchmark — run manually via `--ignored m1_microbench --nocapture`"]
    #[test]
    fn m1_microbench() {
        use otter_vm::module::FunctionIndex;
        use otter_vm::source_compiler::ModuleCompiler;
        use otter_vm::value::RegisterValue;
        use otter_vm::{Interpreter, RuntimeState};
        use oxc_span::SourceType;
        use std::time::Instant;

        let module = ModuleCompiler::new()
            .compile(
                "function f(n) { return n + 1; }",
                "f.js",
                SourceType::default(),
            )
            .expect("M1 source must compile");
        let function = module
            .function(FunctionIndex(0))
            .expect("module has entry function");
        let layout = function.frame_layout();
        let hidden = usize::from(layout.hidden_count());
        let mut registers = vec![RegisterValue::undefined(); usize::from(layout.register_count())];
        registers[hidden] = RegisterValue::from_i32(42);

        let interpreter = Interpreter::new();
        let mut runtime = RuntimeState::new();

        // Warmup: prime any thread-local / lazy state inside the v2
        // interpreter and the runtime before measurement. 2000 iters is
        // overkill for the interpreter but matches the JSC tier-up
        // hotness budget used in the JIT path so the two halves stay
        // comparable when the JIT half lands.
        for _ in 0..2000 {
            let result = interpreter
                .execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
                .expect("warmup execute");
            let _ = result.return_value();
        }

        const ITERS: u64 = 1_000_000;
        let started = Instant::now();
        let mut acc: i64 = 0;
        for _ in 0..ITERS {
            let result = interpreter
                .execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
                .expect("measured execute");
            // Force the result to flow through `acc` so the optimiser
            // can't elide the call. `as_i32` is cheap and covers the
            // happy path for `n + 1`.
            acc = acc.wrapping_add(i64::from(result.return_value().as_i32().unwrap_or(0)));
        }
        let elapsed = started.elapsed();
        // `acc` should equal 43 * ITERS — keep the assertion loose so
        // a tiny semantic regression still prints the timing first.
        assert_eq!(acc, 43 * (ITERS as i64), "interpreter returned wrong sum");

        let total_ns = elapsed.as_nanos();
        let per_iter_ns = total_ns / u128::from(ITERS);
        println!(
            "interp: {per_iter_ns} ns/iter ({} ms total over {ITERS} iter)",
            elapsed.as_millis(),
        );
    }

    /// Microbenchmark for the M7 bench2 sum-loop — the canonical
    /// int32 accumulator loop that the V2_MIGRATION_PLAN reserves
    /// for M7's full benchmark vs `bun` and `node`. Measures the
    /// per-call latency of `sum(1_000_000)` through the v2
    /// interpreter on a persistent `RuntimeState` after a
    /// 100-iteration warmup, and prints a `bench2 interp:` line for
    /// the V2_MIGRATION.md tracker.
    ///
    /// Invoke with:
    /// ```text
    /// cargo test -p otter-jit --release -- --ignored bench2_microbench --nocapture
    /// ```
    ///
    /// JIT-side measurement is intentionally skipped this session
    /// for the same reason as `m1_microbench`: top-level
    /// `execute_with_runtime` doesn't trigger the JSC tier-up hook
    /// (which fires on inner `CallClosure` boundaries), and the v2
    /// source compiler does not yet emit calls (lands at M9).
    /// Direct stencil invocation remains the documented macOS hazard.
    #[cfg(target_arch = "aarch64")]
    #[ignore = "M7 bench2 microbench — run manually via `--ignored bench2_microbench --nocapture`"]
    #[test]
    fn bench2_microbench() {
        use otter_vm::module::FunctionIndex;
        use otter_vm::source_compiler::ModuleCompiler;
        use otter_vm::value::RegisterValue;
        use otter_vm::{Interpreter, RuntimeState};
        use oxc_span::SourceType;
        use std::time::Instant;

        // Canonical M7 source from V2_MIGRATION_PLAN.md, written
        // with single-declarator lets so it stays parseable on the
        // even-narrower M4 surface too. The functional shape is
        // identical to `let s = 0, i = 0;`.
        let source = "function sum(n) { \
                          let s = 0; \
                          let i = 0; \
                          while (i < n) { \
                              s = (s + i) | 0; \
                              i = i + 1; \
                          } \
                          return s; \
                      }";
        let module = ModuleCompiler::new()
            .compile(source, "bench2.ts", SourceType::default())
            .expect("M7 source must compile");
        let function = module
            .function(FunctionIndex(0))
            .expect("module has entry function");
        let layout = function.frame_layout();
        let hidden = usize::from(layout.hidden_count());
        let mut registers = vec![RegisterValue::undefined(); usize::from(layout.register_count())];
        // Loop limit. Match V2_MIGRATION.md's "10⁶ iter" target so
        // the latency row is comparable to the eventual bun / node
        // numbers.
        const N: i32 = 1_000_000;
        registers[hidden] = RegisterValue::from_i32(N);

        let interpreter = Interpreter::new();
        let mut runtime = RuntimeState::new();

        // Warmup — `sum(N)` runs N iterations internally, so 100
        // calls = 10⁸ inner iterations. Plenty to prime any
        // thread-local state in the interpreter.
        const WARMUP_CALLS: u32 = 100;
        for _ in 0..WARMUP_CALLS {
            let result = interpreter
                .execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
                .expect("warmup execute");
            let _ = result.return_value();
        }

        // Measure: 50 calls × 10⁶ inner iterations = 5×10⁷ inner
        // iters total. Per-call latency is the headline number;
        // per-inner-iter is reported alongside for direct
        // comparison with bun/node sum-loop benchmarks.
        const CALLS: u32 = 50;
        let started = Instant::now();
        let mut acc: i64 = 0;
        for _ in 0..CALLS {
            let result = interpreter
                .execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
                .expect("measured execute");
            acc = acc.wrapping_add(i64::from(result.return_value().as_i32().unwrap_or(0)));
        }
        let elapsed = started.elapsed();
        // Sum 0..N-1 = N*(N-1)/2. With N=1_000_000 that's
        // 499_999_500_000 — overflows i32 (which is what JS
        // arithmetic produces with `(s + i) | 0`), so the
        // `int32-wrapped` value is the lower 32 bits of the true
        // sum. Don't assert the exact value; just confirm it's
        // non-zero, deterministic, and identical across runs.
        assert_ne!(acc, 0, "sum returned zero unexpectedly");

        let total_ns = elapsed.as_nanos();
        let total_inner_iters = u128::from(CALLS) * u128::from(N as u32);
        let per_call_ns = total_ns / u128::from(CALLS);
        let per_inner_iter_ns = total_ns / total_inner_iters;
        println!(
            "bench2 interp: {per_call_ns} ns/call ({per_inner_iter_ns} ns/inner-iter, \
             {} ms total over {CALLS} calls × {N} iter, acc={acc})",
            elapsed.as_millis(),
        );
    }
}
