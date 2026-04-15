//! Template baseline Tier 1 candidate analysis.
//!
//! This module is the first step toward a direct `bytecode -> asm` baseline
//! compiler. It does not emit machine code yet; instead, it recognizes a
//! narrow, hot subset of bytecode that can be lowered without MIR/CLIF.

use crate::arch::CodeBuffer;
use otter_vm::bytecode::{Instruction, Opcode};
use otter_vm::module::Function;

/// v2 (Ignition-style accumulator) baseline analyzer — parallel to the
/// v1 analyzer in this module. Lowers a `Function::bytecode_v2()` stream
/// into the existing `TemplateInstruction` IR so the emitter can stay
/// shared between tiers while we validate the v2→baseline plumbing.
/// The full x21-pinned-accumulator emitter lives in Phase 4.2+.
#[cfg(feature = "bytecode_v2")]
pub mod v2;

/// A bytecode operation supported by the template baseline path.
///
/// All int32 binary ops emit a tag guard on each operand — if feedback lies
/// about the types, the stencil bails out to the interpreter instead of
/// corrupting the value domain. "Fused" comparisons (e.g. `LtI32`) require
/// the next instruction to be `JumpIfFalse`; the emitter fuses the compare
/// and branch into a single asm sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateInstruction {
    LoadI32 { dst: u16, imm: i32 },
    Move { dst: u16, src: u16 },
    AddI32 { dst: u16, lhs: u16, rhs: u16 },
    SubI32 { dst: u16, lhs: u16, rhs: u16 },
    MulI32 { dst: u16, lhs: u16, rhs: u16 },
    /// `x | y` — JavaScript `|` operator. Both operands coerce to int32 per
    /// ES §12.12, so with Int32 feedback this becomes a pure 32-bit OR.
    BitOrI32 { dst: u16, lhs: u16, rhs: u16 },
    BitAndI32 { dst: u16, lhs: u16, rhs: u16 },
    BitXorI32 { dst: u16, lhs: u16, rhs: u16 },
    /// `x << y` — JS `<<`. Shift amount masked to low 5 bits (ES §13.9.2).
    ShlI32 { dst: u16, lhs: u16, rhs: u16 },
    /// `x >> y` — signed (arithmetic) right shift.
    ShrI32 { dst: u16, lhs: u16, rhs: u16 },
    /// `x >>> y` — unsigned (logical) right shift.
    UShrI32 { dst: u16, lhs: u16, rhs: u16 },
    /// Comparisons, fused with the following `JumpIfFalse`.
    LtI32 { dst: u16, lhs: u16, rhs: u16 },
    GtI32 { dst: u16, lhs: u16, rhs: u16 },
    GteI32 { dst: u16, lhs: u16, rhs: u16 },
    LteI32 { dst: u16, lhs: u16, rhs: u16 },
    EqI32 { dst: u16, lhs: u16, rhs: u16 },
    /// `ToNumber` speculating the operand is already int32; otherwise bails.
    ToNumberI32 { dst: u16, src: u16 },
    Jump { target_pc: u32 },
    JumpIfFalse { cond: u16, target_pc: u32 },
    Return { src: u16 },
    /// Load the frame's receiver slot ("this" value) into `dst`.
    LoadThis { dst: u16 },
    /// Load the currently-executing closure (from `JitContext::callee_raw`).
    LoadCurrentClosure { dst: u16 },
    /// Load a constant tag value (NaN-boxed immediate).
    LoadTagConst { dst: u16, value: u64 },
    CallDirect { dst: u16, callee_fn_idx: u32, arg_base: u16, arg_count: u16 },
    GetPropShaped { dst: u16, obj: u16, shape_id: u64, slot_index: u16 },
    SetPropShaped { obj: u16, shape_id: u64, slot_index: u16, src: u16 },
}

/// A template-baseline candidate function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateProgram {
    /// Function name for diagnostics.
    pub function_name: String,
    /// Total register count in the shared frame layout.
    pub register_count: u16,
    /// Instructions in template-friendly form.
    pub instructions: Vec<TemplateInstruction>,
    /// Loop headers detected from backward branches.
    pub loop_headers: Vec<u32>,
    /// Per-instruction "trust Int32" flag, indexed by instruction position.
    /// When `true`, the emitter may skip the int32 tag guard on operand loads
    /// because the persistent [`ArithmeticFeedback`] at this PC reports
    /// stable `Int32`. Invariant: if feedback goes non-`Int32` later, the
    /// compiled code must be invalidated and recompiled. Until tier-up
    /// invalidation is wired, we treat "stable Int32" as a hard contract
    /// that the interpreter's monotonic feedback lattice never retracts.
    pub trust_int32: Vec<bool>,
    /// Absolute slot index that — when pinned — is held in callee-saved
    /// register `x21` as a native, unboxed int32 for the lifetime of the
    /// stencil. Eliminates `ldr/extract/...(op)/box/str` round-trips for
    /// every access to that slot in the hot loop body.
    ///
    /// Recognized only for the "accumulator" pattern: the function has
    /// exactly one loop and a single trailing `Return` that reads this
    /// slot. Under that pattern the only paths that observe the slot are
    /// (a) in-loop arithmetic (all writes `x21 = ...`, all reads `... x21`),
    /// (b) `Return` (boxes `x21` into `x0`), (c) bailout (spills `x21` into
    /// the slot via a shared prologue to the bailout pad). Forward loop
    /// exits are not spilled — the pattern guarantees the exit target is
    /// the `Return`, which reads the pinned register directly.
    pub pinned_accumulator: Option<u16>,
}

const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;

/// Why a function is not yet supported by the template baseline Tier 1 path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateCompileError {
    #[error("unsupported opcode at pc {pc}: {opcode:?}")]
    UnsupportedOpcode { pc: u32, opcode: Opcode },
    #[error("jump target out of range at pc {pc}: offset={offset}")]
    InvalidJumpTarget { pc: u32, offset: i32 },
    #[error("missing metadata for CallDirect at pc {pc}")]
    MissingCallMetadata { pc: u32 },
}

/// Why stencil emission failed for an otherwise recognized template program.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateEmitError {
    #[error("unsupported host architecture for template emission: {0}")]
    UnsupportedHostArch(&'static str),
    #[error("register slot offset out of range for template emission: slot={slot}")]
    RegisterSlotOutOfRange { slot: u16 },
    #[error("unsupported template sequence at pc {pc}: {detail}")]
    UnsupportedSequence { pc: u32, detail: &'static str },
    #[error(
        "branch target out of range for template emission: from={source_offset} to pc={target_pc}"
    )]
    BranchTargetOutOfRange { source_offset: u32, target_pc: u32 },
}

/// Analyze whether a function can be compiled by the template baseline path.
///
/// The supported subset is the JSC-Baseline-style int32 fast set:
/// `LoadI32/Move/Add/Sub/Mul/BitOr/BitAnd/BitXor/Shl/Shr/UShr/Lt/Gt/Gte/Lte/Eq/
/// ToNumber/LoadThis/LoadCurrentClosure/LoadHole/LoadUndefined/LoadNull/
/// LoadTrue/LoadFalse/Jump/JumpIfFalse/Return/CallDirect/GetProperty/SetProperty`.
///
/// Passing `feedback = None` disables speculative guard elision (safest for
/// fresh functions with no profile). Passing `Some(fv)` enables
/// `trust_int32` for arithmetic ops whose [`ArithmeticFeedback`] has
/// stabilized at `Int32`.
pub fn analyze_template_candidate(
    function: &Function,
    property_profile: &[Option<otter_vm::PropertyInlineCache>],
) -> Result<TemplateProgram, TemplateCompileError> {
    analyze_template_candidate_with_feedback(function, property_profile, None)
}

/// Feedback-aware analyzer. For each PC that matches an arithmetic or
/// comparison opcode, records whether the persistent feedback has seen
/// *only* Int32 operands — in which case the emitter can skip the tag
/// guard and save ~24 asm insns per arithmetic instruction.
pub fn analyze_template_candidate_with_feedback(
    function: &Function,
    property_profile: &[Option<otter_vm::PropertyInlineCache>],
    feedback: Option<&otter_vm::feedback::FeedbackVector>,
) -> Result<TemplateProgram, TemplateCompileError> {
    use otter_vm::feedback::{ArithmeticFeedback, FeedbackSlotId};

    let instructions = function.bytecode().instructions();
    let mut lowered = Vec::with_capacity(instructions.len());
    let mut loop_headers = Vec::new();
    let mut trust_int32 = Vec::with_capacity(instructions.len());

    for (pc, instruction) in instructions.iter().enumerate() {
        let pc = pc as u32;
        lowered.push(lower_instruction(pc, *instruction, function, property_profile)?);

        // Feedback is indexed by PC per `FrameRuntimeState::feedback_slot_of_kind`.
        let slot = u16::try_from(pc).ok().map(FeedbackSlotId);
        let int32_trusted = match instruction.opcode() {
            Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::BitOr
            | Opcode::BitAnd
            | Opcode::BitXor
            | Opcode::Shl
            | Opcode::Shr
            | Opcode::UShr
            | Opcode::ToNumber => {
                matches!(
                    feedback.zip(slot).and_then(|(fv, id)| fv.arithmetic(id)),
                    Some(ArithmeticFeedback::Int32)
                )
            }
            // Comparisons have their own feedback lattice; keep them guarded
            // for now (they're fewer per loop than arithmetic ops so the
            // guard overhead is less impactful). Phase C can wire this.
            _ => false,
        };
        trust_int32.push(int32_trusted);

        match instruction.opcode() {
            Opcode::Jump | Opcode::JumpIfFalse => {
                let target_pc = resolve_target_pc(pc, instruction.immediate_i32()).ok_or(
                    TemplateCompileError::InvalidJumpTarget {
                        pc,
                        offset: instruction.immediate_i32(),
                    },
                )?;
                if target_pc <= pc && !loop_headers.contains(&target_pc) {
                    loop_headers.push(target_pc);
                }
            }
            _ => {}
        }
    }

    let pinned_accumulator = detect_accumulator_slot(&lowered, &loop_headers);

    Ok(TemplateProgram {
        function_name: function.name().unwrap_or("<anonymous>").to_string(),
        register_count: function.frame_layout().register_count(),
        instructions: lowered,
        loop_headers,
        trust_int32,
        pinned_accumulator,
    })
}

/// Detects the "accumulator" pattern — a single int32 slot that is the sole
/// loop-carried dependency and is returned unchanged by a single trailing
/// `Return`. Pinning this slot to `x21` lets the emitter skip `ldr/tag/
/// extract/.../box/str` for every access inside the hot loop body.
///
/// Pattern requirements:
/// 1. Exactly one loop header.
/// 2. Exactly one `Return` in the program, and it reads a slot `S`.
/// 3. Slot `S` is written inside the loop body (otherwise there's no point).
/// 4. Every write to `S` across the program comes from an int32-producing
///    op (LoadI32, Move, Add, Sub, Mul, BitOr/And/Xor, Shl/Shr/UShr,
///    ToNumberI32). Boxed heap values would corrupt the unboxed register.
///
/// Returns `None` if the pattern doesn't match — the emitter falls back to
/// the plain load/store template.
fn detect_accumulator_slot(
    instructions: &[TemplateInstruction],
    loop_headers: &[u32],
) -> Option<u16> {
    if loop_headers.len() != 1 {
        return None;
    }
    // Find the single Return and the slot it reads.
    let mut return_src: Option<u16> = None;
    for instr in instructions {
        // `CallDirect`, `GetPropShaped`, `SetPropShaped` read/write slot
        // memory out of the emitter's sight (the helper trampolines index
        // the caller's register array directly), so pinning a slot to a
        // register would let stale boxed memory leak through a helper call.
        // Disqualify pinning for any function that contains one of these.
        if matches!(
            instr,
            TemplateInstruction::CallDirect { .. }
                | TemplateInstruction::GetPropShaped { .. }
                | TemplateInstruction::SetPropShaped { .. }
        ) {
            return None;
        }
        if let TemplateInstruction::Return { src } = instr {
            if return_src.is_some() {
                return None; // multiple Returns → bail
            }
            return_src = Some(*src);
        }
    }
    let slot = return_src?;

    // Pinning contract: inside the loop body (`pc >= loop_header`), every
    // write to `slot` must come from an int32-producing op. Outside the
    // loop, non-int32 writes (LoadHole, LoadTagConst, etc.) are tolerated —
    // they are part of the variable-binding prologue and get superseded by
    // the first in-loop int32 write before x21 is actually read. The
    // emitter loads x21 from the slot just before the loop header, so the
    // value there must be a valid int32 by that time.
    let loop_header = loop_headers[0];
    let mut has_loop_write = false;
    for (idx, instr) in instructions.iter().enumerate() {
        let pc = idx as u32;
        let in_loop = pc >= loop_header;
        let writes_slot_int32_ok = match instr {
            TemplateInstruction::LoadI32 { dst, .. }
            | TemplateInstruction::Move { dst, .. }
            | TemplateInstruction::AddI32 { dst, .. }
            | TemplateInstruction::SubI32 { dst, .. }
            | TemplateInstruction::MulI32 { dst, .. }
            | TemplateInstruction::BitOrI32 { dst, .. }
            | TemplateInstruction::BitAndI32 { dst, .. }
            | TemplateInstruction::BitXorI32 { dst, .. }
            | TemplateInstruction::ShlI32 { dst, .. }
            | TemplateInstruction::ShrI32 { dst, .. }
            | TemplateInstruction::UShrI32 { dst, .. }
            | TemplateInstruction::ToNumberI32 { dst, .. } => *dst == slot,
            TemplateInstruction::LoadThis { dst }
            | TemplateInstruction::LoadCurrentClosure { dst }
            | TemplateInstruction::LoadTagConst { dst, .. }
            | TemplateInstruction::GetPropShaped { dst, .. } => {
                // Non-int32 writes to the pinned slot are only OK before the
                // loop. Inside the loop they would corrupt the pinned reg.
                if *dst == slot && in_loop {
                    return None;
                }
                false
            }
            TemplateInstruction::LtI32 { dst, .. }
            | TemplateInstruction::GtI32 { dst, .. }
            | TemplateInstruction::GteI32 { dst, .. }
            | TemplateInstruction::LteI32 { dst, .. }
            | TemplateInstruction::EqI32 { dst, .. } => {
                // Comparisons write a bool into dst — pattern doesn't want
                // a pinned *int32* slot to receive a bool, ever.
                if *dst == slot {
                    return None;
                }
                false
            }
            TemplateInstruction::CallDirect { .. }
            | TemplateInstruction::Jump { .. }
            | TemplateInstruction::JumpIfFalse { .. }
            | TemplateInstruction::Return { .. }
            | TemplateInstruction::SetPropShaped { .. } => false,
        };
        if writes_slot_int32_ok && in_loop {
            has_loop_write = true;
        }
    }

    has_loop_write.then_some(slot)
}

/// Emit an architecture-specific baseline stencil for a recognized template program.
///
/// This is a code-buffer generator, not yet an installed executable function.
/// The first implementation targets the host `aarch64` baseline subset used by
/// hot arithmetic loops.
pub fn emit_template_stencil(program: &TemplateProgram) -> Result<CodeBuffer, TemplateEmitError> {
    #[cfg(target_arch = "aarch64")]
    {
        emit_template_stencil_aarch64(program)
    }
    #[cfg(target_arch = "x86_64")]
    {
        emit_template_stencil_x86_64(program)
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        Err(TemplateEmitError::UnsupportedHostArch(
            std::env::consts::ARCH,
        ))
    }
}

#[cfg(target_arch = "aarch64")]
fn emit_template_stencil_aarch64(
    program: &TemplateProgram,
) -> Result<CodeBuffer, TemplateEmitError> {
    use crate::arch::aarch64::{Assembler, Cond, Reg};

    #[derive(Debug, Clone, Copy)]
    enum BranchKind {
        Unconditional,
        Conditional(Cond),
    }

    #[derive(Debug, Clone, Copy)]
    struct BranchPatch {
        source_offset: u32,
        target_pc: u32,
        kind: BranchKind,
    }

    #[derive(Debug, Clone, Copy)]
    struct BailoutPatch {
        source_offset: u32,
        pc: u32,
        reason: crate::BailoutReason,
    }

    fn slot_offset(slot: u16) -> Result<u32, TemplateEmitError> {
        let byte_offset = u32::from(slot) * 8;
        if byte_offset > (4095 * 8) {
            return Err(TemplateEmitError::RegisterSlotOutOfRange { slot });
        }
        Ok(byte_offset)
    }

    /// Boxes the signed int32 held in `src_unboxed` and stores it into the
    /// slot at `slot_off`. Companion to [`load_int32`] for emitting the
    /// `box_int32 + str` tail of an arithmetic op when the destination
    /// slot is **not** the pinned accumulator.
    fn store_boxed_int32(
        asm: &mut Assembler,
        src_unboxed: Reg,
        slot_off: u32,
    ) -> Result<(), TemplateEmitError> {
        asm.box_int32(Reg::X10, src_unboxed);
        asm.str_u64_imm(Reg::X10, Reg::X9, slot_off);
        Ok(())
    }

    /// Loads a boxed value from `slot_off` into `dst`, optionally guards the
    /// int32 tag, and sign-extends the 32-bit payload into the 64-bit
    /// destination register.
    ///
    /// Sign extension is uniform — it's correct for signed comparisons and
    /// arithmetic right shift, and the high 32 bits are discarded anyway by
    /// `box_int32`/W-register shifts/low-32 truncation on store, so simple
    /// arithmetic is unaffected by the choice of extension.
    ///
    /// `trust_int32=true` skips the tag check and the bailout branch — safe
    /// only when the persistent feedback for this PC has stabilized at
    /// `ArithmeticFeedback::Int32`. That saves ~3 asm instructions per
    /// operand on hot arithmetic (xor, tst, b.ne).
    fn load_int32(
        asm: &mut Assembler,
        dst: Reg,
        slot_off: u32,
        pc: u32,
        bailout_patches: &mut Vec<BailoutPatch>,
        trust_int32: bool,
    ) {
        asm.ldr_u64_imm(dst, Reg::X9, slot_off);
        if !trust_int32 {
            // 3-insn XOR+TST path. Reads the preloaded `TAG_INT32` out of
            // x20 (pinned once in the prologue), XORs into x14, tests the
            // upper 32 bits. `Ne` means "not an int32", which branches to
            // the bailout pad.
            asm.check_int32_tag_fast(dst, Reg::X20);
            let bp = asm.b_cond_placeholder(Cond::Ne);
            bailout_patches.push(BailoutPatch {
                source_offset: bp,
                pc,
                reason: crate::BailoutReason::TypeGuardFailed,
            });
        }
        asm.sxtw(dst, dst);
    }

    /// Emits a fused int32 comparison + conditional branch. The branch fires
    /// when the comparison is **false** (JumpIfFalse semantics). `branch_cond`
    /// is the condition code that represents the **negation** of the JS
    /// operator (e.g. `Lt` → branch on `Ge`).
    fn emit_fused_compare_branch(
        asm: &mut Assembler,
        program: &TemplateProgram,
        pc: usize,
        lhs: u16,
        rhs: u16,
        branch_cond: Cond,
        pc_offsets: &mut [u32],
        patches: &mut Vec<BranchPatch>,
        bailout_patches: &mut Vec<BailoutPatch>,
    ) -> Result<usize, TemplateEmitError> {
        let Some(TemplateInstruction::JumpIfFalse { target_pc, .. }) =
            program.instructions.get(pc + 1)
        else {
            return Err(TemplateEmitError::UnsupportedSequence {
                pc: pc as u32,
                detail: "comparison requires immediate `JumpIfFalse` fusion",
            });
        };

        let trust = program.trust_int32.get(pc).copied().unwrap_or(false);
        if program.pinned_accumulator == Some(lhs) {
            asm.mov_rr(Reg::X10, Reg::X21);
        } else {
            load_int32(asm, Reg::X10, slot_offset(lhs)?, pc as u32, bailout_patches, trust);
        }
        if program.pinned_accumulator == Some(rhs) {
            asm.mov_rr(Reg::X11, Reg::X21);
        } else {
            load_int32(asm, Reg::X11, slot_offset(rhs)?, pc as u32, bailout_patches, trust);
        }
        asm.cmp_rr(Reg::X10, Reg::X11);
        let branch = asm.b_cond_placeholder(branch_cond);
        patches.push(BranchPatch {
            source_offset: branch,
            target_pc: *target_pc,
            kind: BranchKind::Conditional(branch_cond),
        });
        if pc + 1 < pc_offsets.len() {
            pc_offsets[pc + 1] = asm.position();
        }
        Ok(pc + 1)
    }

    let mut buf = CodeBuffer::new();
    let mut asm = Assembler::new(&mut buf);
    let mut pc_offsets = vec![0_u32; program.instructions.len()];
    let mut patches = Vec::new();
    let mut bailout_patches = Vec::new();

    // Prologue: 32-byte frame saving x19, lr, and x20.
    // x19 pins the `JitContext*` for the whole stencil so we don't need to
    // re-fetch it across helper calls (helpers follow AAPCS and preserve
    // callee-saved registers, x19-x28).
    // x20 is pinned to `TAG_INT32`, reused by every `check_int32_tag_fast`
    // so each tag guard collapses to 3 asm instructions.
    // x21 is optionally pinned to the unboxed int32 value of a single
    // accumulator slot for the whole stencil (see `detect_accumulator_slot`)
    // so every arithmetic op on that slot becomes a direct register op
    // instead of an ldr/tag/extract/op/box/str round-trip.
    asm.push_x19_lr_32();
    asm.str_x20_at_sp16();
    // x19 = JitContext*
    asm.mov_rr(Reg::X19, Reg::X0);
    // x9 = registers_base (hot, reused every instruction)
    asm.ldr_u64_imm(Reg::X9, Reg::X19, 0);
    // x20 = TAG_INT32 (preloaded once for fast tag checks)
    asm.mov_imm64(Reg::X20, TAG_INT32);

    // The pinned accumulator is **never** loaded with an explicit
    // prologue ldr. x21 is initialized by the first pinned-aware write to
    // the slot — LoadI32, Move(dst=pinned), or an arithmetic op that
    // writes into it — because those are exactly the opcodes our detector
    // accepts in the program's write set. Before that first write nothing
    // reads x21, so its initial garbage is harmless. This is the only
    // correct design: an eager re-load at the loop header would race with
    // stale slot memory (e.g. the `LoadHole` that introduces the
    // variable binding sets the slot to `TAG_HOLE` until the first real
    // write, but our pinned-aware writes deliberately skip memory).
    let pinned_acc_slot = program.pinned_accumulator;
    let pinned_acc_slot_off = pinned_acc_slot.map(slot_offset).transpose()?;

    let mut pc = 0usize;
    while pc < program.instructions.len() {
        pc_offsets[pc] = asm.position();

        match &program.instructions[pc] {
            TemplateInstruction::LoadI32 { dst, imm } => {
                if pinned_acc_slot == Some(*dst) {
                    // Pinned: keep sign-extended int32 in x21.
                    asm.mov_imm64(Reg::X21, *imm as i64 as u64);
                } else {
                    let boxed = TAG_INT32 | u64::from(*imm as u32);
                    asm.mov_imm64(Reg::X10, boxed);
                    asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
                }
            }
            TemplateInstruction::Move { dst, src } => {
                if pinned_acc_slot == Some(*dst) && pinned_acc_slot == Some(*src) {
                    // no-op
                } else if pinned_acc_slot == Some(*dst) {
                    // Load src, tag-guard, sxtw into x21.
                    load_int32(
                        &mut asm,
                        Reg::X21,
                        slot_offset(*src)?,
                        pc as u32,
                        &mut bailout_patches,
                        /* trust_int32 */ false,
                    );
                } else if pinned_acc_slot == Some(*src) {
                    // Box x21 and store to dst slot.
                    store_boxed_int32(&mut asm, Reg::X21, slot_offset(*dst)?)?;
                } else {
                    asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*src)?);
                    asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
                }
            }
            TemplateInstruction::LoadThis { dst } => {
                asm.ldr_u64_imm(
                    Reg::X10,
                    Reg::X19,
                    crate::context::offsets::THIS_RAW as u32,
                );
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::LoadCurrentClosure { dst } => {
                asm.ldr_u64_imm(
                    Reg::X10,
                    Reg::X19,
                    crate::context::offsets::CALLEE_RAW as u32,
                );
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::LoadTagConst { dst, value } => {
                asm.mov_imm64(Reg::X10, *value);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::ToNumberI32 { dst, src } => {
                // Speculate int32: guard tag, then copy boxed value as-is.
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*src)?);
                asm.check_int32_tag(Reg::X10);
                let bp = asm.b_cond_placeholder(Cond::Ne);
                bailout_patches.push(BailoutPatch {
                    source_offset: bp,
                    pc: pc as u32,
                    reason: crate::BailoutReason::TypeGuardFailed,
                });
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::AddI32 { dst, lhs, rhs } => {
                let trust = program.trust_int32.get(pc).copied().unwrap_or(false);
                // Load lhs into X10 (from x21 if pinned, else from memory).
                if pinned_acc_slot == Some(*lhs) {
                    asm.mov_rr(Reg::X10, Reg::X21);
                } else {
                    load_int32(
                        &mut asm, Reg::X10, slot_offset(*lhs)?, pc as u32,
                        &mut bailout_patches, trust,
                    );
                }
                // Load rhs into X11.
                if pinned_acc_slot == Some(*rhs) {
                    asm.mov_rr(Reg::X11, Reg::X21);
                } else {
                    load_int32(
                        &mut asm, Reg::X11, slot_offset(*rhs)?, pc as u32,
                        &mut bailout_patches, trust,
                    );
                }
                asm.add_rrr(Reg::X10, Reg::X10, Reg::X11);
                // Store result. If dst is pinned, keep it unboxed in x21
                // (sxtw re-signs the 32-bit result for later signed ops).
                if pinned_acc_slot == Some(*dst) {
                    asm.sxtw(Reg::X21, Reg::X10);
                } else {
                    asm.box_int32(Reg::X10, Reg::X10);
                    asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
                }
            }
            TemplateInstruction::SubI32 { dst, lhs, rhs } => {
                let trust = program.trust_int32.get(pc).copied().unwrap_or(false);
                if pinned_acc_slot == Some(*lhs) {
                    asm.mov_rr(Reg::X10, Reg::X21);
                } else {
                    load_int32(&mut asm, Reg::X10, slot_offset(*lhs)?, pc as u32, &mut bailout_patches, trust);
                }
                if pinned_acc_slot == Some(*rhs) {
                    asm.mov_rr(Reg::X11, Reg::X21);
                } else {
                    load_int32(&mut asm, Reg::X11, slot_offset(*rhs)?, pc as u32, &mut bailout_patches, trust);
                }
                asm.sub_rrr(Reg::X10, Reg::X10, Reg::X11);
                if pinned_acc_slot == Some(*dst) {
                    asm.sxtw(Reg::X21, Reg::X10);
                } else {
                    asm.box_int32(Reg::X10, Reg::X10);
                    asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
                }
            }
            TemplateInstruction::MulI32 { dst, lhs, rhs } => {
                let trust = program.trust_int32.get(pc).copied().unwrap_or(false);
                if pinned_acc_slot == Some(*lhs) {
                    asm.mov_rr(Reg::X10, Reg::X21);
                } else {
                    load_int32(&mut asm, Reg::X10, slot_offset(*lhs)?, pc as u32, &mut bailout_patches, trust);
                }
                if pinned_acc_slot == Some(*rhs) {
                    asm.mov_rr(Reg::X11, Reg::X21);
                } else {
                    load_int32(&mut asm, Reg::X11, slot_offset(*rhs)?, pc as u32, &mut bailout_patches, trust);
                }
                asm.mul_rrr(Reg::X10, Reg::X10, Reg::X11);
                if pinned_acc_slot == Some(*dst) {
                    asm.sxtw(Reg::X21, Reg::X10);
                } else {
                    asm.box_int32(Reg::X10, Reg::X10);
                    asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
                }
            }
            TemplateInstruction::BitOrI32 { dst, lhs, rhs } => {
                let trust = program.trust_int32.get(pc).copied().unwrap_or(false);
                if pinned_acc_slot == Some(*lhs) {
                    asm.mov_rr(Reg::X10, Reg::X21);
                } else {
                    load_int32(&mut asm, Reg::X10, slot_offset(*lhs)?, pc as u32, &mut bailout_patches, trust);
                }
                if pinned_acc_slot == Some(*rhs) {
                    asm.mov_rr(Reg::X11, Reg::X21);
                } else {
                    load_int32(&mut asm, Reg::X11, slot_offset(*rhs)?, pc as u32, &mut bailout_patches, trust);
                }
                asm.orr_rrr(Reg::X10, Reg::X10, Reg::X11);
                if pinned_acc_slot == Some(*dst) {
                    asm.sxtw(Reg::X21, Reg::X10);
                } else {
                    asm.box_int32(Reg::X10, Reg::X10);
                    asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
                }
            }
            TemplateInstruction::BitAndI32 { dst, lhs, rhs } => {
                load_int32(
                    &mut asm,
                    Reg::X10,
                    slot_offset(*lhs)?,
                    pc as u32,
                    &mut bailout_patches,
                    program.trust_int32.get(pc).copied().unwrap_or(false),
                );
                load_int32(
                    &mut asm,
                    Reg::X11,
                    slot_offset(*rhs)?,
                    pc as u32,
                    &mut bailout_patches,
                    program.trust_int32.get(pc).copied().unwrap_or(false),
                );
                asm.and_rrr(Reg::X10, Reg::X10, Reg::X11);
                asm.box_int32(Reg::X10, Reg::X10);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::BitXorI32 { dst, lhs, rhs } => {
                load_int32(
                    &mut asm,
                    Reg::X10,
                    slot_offset(*lhs)?,
                    pc as u32,
                    &mut bailout_patches,
                    program.trust_int32.get(pc).copied().unwrap_or(false),
                );
                load_int32(
                    &mut asm,
                    Reg::X11,
                    slot_offset(*rhs)?,
                    pc as u32,
                    &mut bailout_patches,
                    program.trust_int32.get(pc).copied().unwrap_or(false),
                );
                asm.eor_rrr(Reg::X10, Reg::X10, Reg::X11);
                asm.box_int32(Reg::X10, Reg::X10);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::ShlI32 { dst, lhs, rhs } => {
                load_int32(
                    &mut asm,
                    Reg::X10,
                    slot_offset(*lhs)?,
                    pc as u32,
                    &mut bailout_patches,
                    program.trust_int32.get(pc).copied().unwrap_or(false),
                );
                load_int32(
                    &mut asm,
                    Reg::X11,
                    slot_offset(*rhs)?,
                    pc as u32,
                    &mut bailout_patches,
                    program.trust_int32.get(pc).copied().unwrap_or(false),
                );
                asm.lslv_w(Reg::X10, Reg::X10, Reg::X11);
                asm.box_int32(Reg::X10, Reg::X10);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::ShrI32 { dst, lhs, rhs } => {
                // Signed (arithmetic) shift: sign-extend lhs before shifting.
                load_int32(
                    &mut asm,
                    Reg::X10,
                    slot_offset(*lhs)?,
                    pc as u32,
                    &mut bailout_patches,
                    program.trust_int32.get(pc).copied().unwrap_or(false),
                );
                load_int32(
                    &mut asm,
                    Reg::X11,
                    slot_offset(*rhs)?,
                    pc as u32,
                    &mut bailout_patches,
                    program.trust_int32.get(pc).copied().unwrap_or(false),
                );
                asm.asrv_w(Reg::X10, Reg::X10, Reg::X11);
                asm.box_int32(Reg::X10, Reg::X10);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::UShrI32 { dst, lhs, rhs } => {
                load_int32(
                    &mut asm,
                    Reg::X10,
                    slot_offset(*lhs)?,
                    pc as u32,
                    &mut bailout_patches,
                    program.trust_int32.get(pc).copied().unwrap_or(false),
                );
                load_int32(
                    &mut asm,
                    Reg::X11,
                    slot_offset(*rhs)?,
                    pc as u32,
                    &mut bailout_patches,
                    program.trust_int32.get(pc).copied().unwrap_or(false),
                );
                asm.lsrv_w(Reg::X10, Reg::X10, Reg::X11);
                asm.box_int32(Reg::X10, Reg::X10);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::LtI32 { lhs, rhs, .. } => {
                pc = emit_fused_compare_branch(
                    &mut asm,
                    program,
                    pc,
                    *lhs,
                    *rhs,
                    Cond::Ge, // branch when NOT (lhs < rhs)
                    &mut pc_offsets,
                    &mut patches,
                    &mut bailout_patches,
                )?;
            }
            TemplateInstruction::GtI32 { lhs, rhs, .. } => {
                pc = emit_fused_compare_branch(
                    &mut asm,
                    program,
                    pc,
                    *lhs,
                    *rhs,
                    Cond::Le, // branch when NOT (lhs > rhs)
                    &mut pc_offsets,
                    &mut patches,
                    &mut bailout_patches,
                )?;
            }
            TemplateInstruction::GteI32 { lhs, rhs, .. } => {
                pc = emit_fused_compare_branch(
                    &mut asm,
                    program,
                    pc,
                    *lhs,
                    *rhs,
                    Cond::Lt, // branch when NOT (lhs >= rhs)
                    &mut pc_offsets,
                    &mut patches,
                    &mut bailout_patches,
                )?;
            }
            TemplateInstruction::LteI32 { lhs, rhs, .. } => {
                pc = emit_fused_compare_branch(
                    &mut asm,
                    program,
                    pc,
                    *lhs,
                    *rhs,
                    Cond::Gt, // branch when NOT (lhs <= rhs)
                    &mut pc_offsets,
                    &mut patches,
                    &mut bailout_patches,
                )?;
            }
            TemplateInstruction::EqI32 { lhs, rhs, .. } => {
                pc = emit_fused_compare_branch(
                    &mut asm,
                    program,
                    pc,
                    *lhs,
                    *rhs,
                    Cond::Ne, // branch when NOT (lhs == rhs)
                    &mut pc_offsets,
                    &mut patches,
                    &mut bailout_patches,
                )?;
            }
            TemplateInstruction::JumpIfFalse { .. } => {
                return Err(TemplateEmitError::UnsupportedSequence {
                    pc: pc as u32,
                    detail:
                        "standalone `JumpIfFalse` is not yet supported; use compare/branch fusion",
                });
            }
            TemplateInstruction::Jump { target_pc } => {
                let branch = asm.b_placeholder();
                patches.push(BranchPatch {
                    source_offset: branch,
                    target_pc: *target_pc,
                    kind: BranchKind::Unconditional,
                });
            }
            TemplateInstruction::CallDirect { dst, callee_fn_idx, arg_base, arg_count } => {
                // Prepare arguments across X0..X4 for C ABI.
                // X0 = ctx (from X19)
                asm.mov_rr(Reg::X0, Reg::X19);
                // X1 = callee_fn_idx
                asm.mov_imm64(Reg::X1, u64::from(*callee_fn_idx));
                // X2 = arg_base
                asm.mov_imm64(Reg::X2, u64::from(*arg_base));
                // X3 = arg_count
                asm.mov_imm64(Reg::X3, u64::from(*arg_count));
                // X4 = bytecode_pc
                asm.mov_imm64(Reg::X4, pc as u64);

                let helper_ptr = crate::runtime_helpers::otter_baseline_call_direct as *const () as u64;
                asm.mov_imm64(Reg::X10, helper_ptr);
                asm.blr(Reg::X10);

                // C function call might clobber X9. Reload registers_base.
                asm.ldr_u64_imm(Reg::X9, Reg::X19, 0);

                // Return value is in X0. Store it into `dst` slot.
                asm.str_u64_imm(Reg::X0, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::Return { src } => {
                if pinned_acc_slot == Some(*src) {
                    // Box the pinned unboxed int32 in x21 straight into x0.
                    asm.box_int32(Reg::X0, Reg::X21);
                } else {
                    asm.ldr_u64_imm(Reg::X0, Reg::X9, slot_offset(*src)?);
                }
                asm.ldr_x20_at_sp16();
                asm.pop_x19_lr_32();
                asm.ret();
            }
            TemplateInstruction::GetPropShaped { dst, obj, shape_id, slot_index } => {
                // 1. Load boxed object from slot
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*obj)?);
                
                // 2. Check object tag
                asm.check_object_tag(Reg::X10);
                let jump_invalid_type = asm.b_cond_placeholder(Cond::Ne);
                
                // 3. Extract handle (lower 32 bits)
                asm.extract_int32(Reg::X10, Reg::X10);
                
                // 4. Resolve object pointer: slots_base + handle * 32
                asm.ldr_u64_imm(Reg::X11, Reg::X19, crate::context::offsets::HEAP_SLOTS_BASE as u32);
                asm.add_rrr_lsl(Reg::X11, Reg::X11, Reg::X10, 5);
                asm.ldr_u64_imm(Reg::X12, Reg::X11, 0); // Load Box<JsObject> data pointer
                
                // 5. Shape guard
                asm.ldr_u64_imm(Reg::X13, Reg::X12, crate::context::offsets::js_object::SHAPE_ID as u32);
                asm.mov_imm64(Reg::X14, *shape_id);
                asm.cmp_rr(Reg::X13, Reg::X14);
                let jump_shape_mismatch = asm.b_cond_placeholder(Cond::Ne);
                
                // 6. Load from values buffer: values.as_ptr() is at offset 48
                // Values are PropertyValue (24 bytes). Data value is at offset 8.
                asm.ldr_u64_imm(Reg::X13, Reg::X12, crate::context::offsets::js_object::VALUES_PTR as u32);
                let offset = u64::from(*slot_index) * 24 + 8;
                asm.mov_imm64(Reg::X14, offset);
                asm.add_rrr(Reg::X13, Reg::X13, Reg::X14);
                asm.ldr_u64_imm(Reg::X10, Reg::X13, 0);
                
                // 7. Store result
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);

                bailout_patches.push(BailoutPatch {
                    source_offset: jump_invalid_type,
                    pc: pc as u32,
                    reason: crate::BailoutReason::TypeGuardFailed,
                });
                bailout_patches.push(BailoutPatch {
                    source_offset: jump_shape_mismatch,
                    pc: pc as u32,
                    reason: crate::BailoutReason::ShapeGuardFailed,
                });

            }
            TemplateInstruction::SetPropShaped { obj, shape_id, slot_index, src } => {
                let slot_idx_val = *slot_index as u64; 
                // 1. Load boxed object from slot
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*obj)?);

                // 2. Check object tag
                asm.check_object_tag(Reg::X10);
                let jump_invalid_type = asm.b_cond_placeholder(Cond::Ne);

                // 3. Extract handle (lower 32 bits)
                asm.extract_int32(Reg::X10, Reg::X10);

                // 4. Resolve object pointer: slots_base + handle * 32
                asm.ldr_u64_imm(Reg::X11, Reg::X19, crate::context::offsets::HEAP_SLOTS_BASE as u32);
                asm.add_rrr_lsl(Reg::X11, Reg::X11, Reg::X10, 5);
                asm.ldr_u64_imm(Reg::X12, Reg::X11, 0); // Load Box<JsObject> data pointer

                // 5. Shape guard
                asm.ldr_u64_imm(Reg::X13, Reg::X12, crate::context::offsets::js_object::SHAPE_ID as u32);
                asm.mov_imm64(Reg::X14, *shape_id);
                asm.cmp_rr(Reg::X13, Reg::X14);
                let jump_shape_mismatch = asm.b_cond_placeholder(Cond::Ne);

                // 6. Load SRC value from its slot
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*src)?);

                // 7. Write to values buffer: values.as_ptr() is at offset 48
                // Values are PropertyValue (24 bytes). Data value is at offset 8.
                asm.ldr_u64_imm(Reg::X13, Reg::X12, crate::context::offsets::js_object::VALUES_PTR as u32);
                let offset = slot_idx_val * 24 + 8;
                if offset <= 4095 * 8 && offset % 8 == 0 {
                    asm.str_u64_imm(Reg::X10, Reg::X13, offset as u32);
                } else {
                    asm.mov_imm64(Reg::X14, offset);
                    asm.add_rrr(Reg::X13, Reg::X13, Reg::X14);
                    asm.str_u64_imm(Reg::X10, Reg::X13, 0);
                }

                // 8. Write barrier: temporarily skipped for performance and since it is currently a NO-OP.
                // TODO: Re-enable once generational GC is active.

                // Bailout patches
                bailout_patches.push(BailoutPatch {
                    source_offset: jump_invalid_type,
                    pc: pc as u32,
                    reason: crate::BailoutReason::TypeGuardFailed,
                });
                bailout_patches.push(BailoutPatch {
                    source_offset: jump_shape_mismatch,
                    pc: pc as u32,
                    reason: crate::BailoutReason::ShapeGuardFailed,
                });
            }
        }

        pc += 1;
    }

    // Shared bailout block.
    //
    // Per-site pads jump here with X10 = bailout_pc and X11 = reason.
    // The pads DO NOT spill x21 — we spill once here before surrendering
    // control back to the interpreter so it observes the current value of
    // the pinned accumulator through slot memory. Boxing clobbers x12, not
    // x10/x11, so the pc/reason survive the spill.
    let bailout_common = asm.position();
    if let (Some(slot), Some(slot_off)) = (pinned_acc_slot, pinned_acc_slot_off) {
        let _ = slot;
        // x12 = box_int32(x21); str x12, [x9, #slot_off]
        asm.box_int32(Reg::X12, Reg::X21);
        asm.str_u64_imm(Reg::X12, Reg::X9, slot_off);
    }
    asm.str_u32_imm(Reg::X10, Reg::X19, crate::context::offsets::BAILOUT_PC as u32);
    asm.str_u32_imm(Reg::X11, Reg::X19, crate::context::offsets::BAILOUT_REASON as u32);
    asm.mov_imm64(Reg::X0, crate::BAILOUT_SENTINEL);
    asm.ldr_x20_at_sp16();
    asm.pop_x19_lr_32();
    asm.ret();


    // Drop assembler to allow patching the buffer directly
    std::mem::drop(asm);

    for patch in patches {
        let Some(&target_offset) = pc_offsets.get(patch.target_pc as usize) else {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_offset: patch.source_offset,
                target_pc: patch.target_pc,
            });
        };
        let delta = i64::from(target_offset) - i64::from(patch.source_offset);
        if delta % 4 != 0 {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_offset: patch.source_offset,
                target_pc: patch.target_pc,
            });
        }
        let Some(existing) = buf.read_u32_le(patch.source_offset) else {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_offset: patch.source_offset,
                target_pc: patch.target_pc,
            });
        };
        let patched = match patch.kind {
            BranchKind::Unconditional => {
                let imm26 = ((delta / 4) as i32 as u32) & 0x03FF_FFFF;
                existing | imm26
            }
            BranchKind::Conditional(_) => {
                // Conditional branch placeholder already carries the `cond`
                // bits in the opcode base; we only need to patch the 19-bit
                // PC-relative offset. The condition was baked in when the
                // placeholder was emitted.
                let imm19 = ((delta / 4) as i32 as u32) & 0x0007_FFFF;
                existing | (imm19 << 5)
            }
        };
        if !buf.patch_u32_le(patch.source_offset, patched) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_offset: patch.source_offset,
                target_pc: patch.target_pc,
            });
        }
    }

    for patch in bailout_patches {
        let pad_offset = buf.len() as u32;
        // Patch the guard to jump to this pad
        let delta = i64::from(pad_offset) - i64::from(patch.source_offset);
        let existing = buf.read_u32_le(patch.source_offset).unwrap();
        let imm19 = ((delta / 4) as i32 as u32) & 0x0007_FFFF;
        buf.patch_u32_le(patch.source_offset, existing | (imm19 << 5));

        // Create a new assembler for the pad
        let mut pad_asm = Assembler::new(&mut buf);
        // Pad sequence: set PC and Reason, then jump to common bailout
        pad_asm.mov_imm64(Reg::X10, u64::from(patch.pc));
        pad_asm.mov_imm64(Reg::X11, patch.reason as u64);
        let jump_common = pad_asm.b_placeholder();
        std::mem::drop(pad_asm); // Drop to patch

        let common_delta = i64::from(bailout_common) - i64::from(jump_common);
        let imm26 = ((common_delta / 4) as i32 as u32) & 0x03FF_FFFF;
        buf.patch_u32_le(jump_common, 0x14000000 | imm26);
    }

    Ok(buf)
}

fn lower_instruction(
    pc: u32,
    instruction: Instruction,
    function: &Function,
    profile: &[Option<otter_vm::PropertyInlineCache>],
) -> Result<TemplateInstruction, TemplateCompileError> {
    // Bytecode register indices are user-visible (0..user_visible_count);
    // the interpreter resolves them to absolute frame slots by adding
    // `hidden_count` (receiver + internal reserved slots). The template
    // emitter writes directly to `registers_base + slot * 8`, so we MUST
    // translate to absolute slots here or the stencil reads/writes past
    // parameters into the wrong memory — which is how inner functions with
    // `hidden_count > 0` infinite-loop after a Phase-A tier-up.
    let resolve = |reg: u16| -> Result<u16, TemplateCompileError> {
        function
            .frame_layout()
            .resolve_user_visible(reg)
            .ok_or(TemplateCompileError::UnsupportedOpcode {
                pc,
                opcode: instruction.opcode(),
            })
    };
    match instruction.opcode() {
        Opcode::LoadI32 => Ok(TemplateInstruction::LoadI32 {
            dst: resolve(instruction.a())?,
            imm: instruction.immediate_i32(),
        }),
        Opcode::Move => Ok(TemplateInstruction::Move {
            dst: resolve(instruction.a())?,
            src: resolve(instruction.b())?,
        }),
        Opcode::Add => Ok(TemplateInstruction::AddI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::Sub => Ok(TemplateInstruction::SubI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::Mul => Ok(TemplateInstruction::MulI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::Lt => Ok(TemplateInstruction::LtI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::Gt => Ok(TemplateInstruction::GtI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::Gte => Ok(TemplateInstruction::GteI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::Lte => Ok(TemplateInstruction::LteI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::Eq => Ok(TemplateInstruction::EqI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::BitOr => Ok(TemplateInstruction::BitOrI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::BitAnd => Ok(TemplateInstruction::BitAndI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::BitXor => Ok(TemplateInstruction::BitXorI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::Shl => Ok(TemplateInstruction::ShlI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::Shr => Ok(TemplateInstruction::ShrI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::UShr => Ok(TemplateInstruction::UShrI32 {
            dst: resolve(instruction.a())?,
            lhs: resolve(instruction.b())?,
            rhs: resolve(instruction.c())?,
        }),
        Opcode::ToNumber => Ok(TemplateInstruction::ToNumberI32 {
            dst: resolve(instruction.a())?,
            src: resolve(instruction.b())?,
        }),
        Opcode::LoadThis => Ok(TemplateInstruction::LoadThis {
            dst: resolve(instruction.a())?,
        }),
        Opcode::LoadCurrentClosure => Ok(TemplateInstruction::LoadCurrentClosure {
            dst: resolve(instruction.a())?,
        }),
        Opcode::LoadHole => Ok(TemplateInstruction::LoadTagConst {
            dst: resolve(instruction.a())?,
            value: otter_vm::value::TAG_HOLE,
        }),
        Opcode::LoadUndefined => Ok(TemplateInstruction::LoadTagConst {
            dst: resolve(instruction.a())?,
            value: otter_vm::value::TAG_UNDEFINED,
        }),
        Opcode::LoadNull => Ok(TemplateInstruction::LoadTagConst {
            dst: resolve(instruction.a())?,
            value: otter_vm::value::TAG_NULL,
        }),
        Opcode::LoadTrue => Ok(TemplateInstruction::LoadTagConst {
            dst: resolve(instruction.a())?,
            value: otter_vm::value::TAG_TRUE,
        }),
        Opcode::LoadFalse => Ok(TemplateInstruction::LoadTagConst {
            dst: resolve(instruction.a())?,
            value: otter_vm::value::TAG_FALSE,
        }),
        Opcode::Jump => Ok(TemplateInstruction::Jump {
            target_pc: resolve_target_pc(pc, instruction.immediate_i32()).ok_or(
                TemplateCompileError::InvalidJumpTarget {
                    pc,
                    offset: instruction.immediate_i32(),
                },
            )?,
        }),
        Opcode::JumpIfFalse => Ok(TemplateInstruction::JumpIfFalse {
            cond: resolve(instruction.a())?,
            target_pc: resolve_target_pc(pc, instruction.immediate_i32()).ok_or(
                TemplateCompileError::InvalidJumpTarget {
                    pc,
                    offset: instruction.immediate_i32(),
                },
            )?,
        }),
        Opcode::Return => Ok(TemplateInstruction::Return {
            src: resolve(instruction.a())?,
        }),
        Opcode::CallDirect => {
            let call = function.calls().get_direct(pc).ok_or(
                TemplateCompileError::MissingCallMetadata { pc }
            )?;
            Ok(TemplateInstruction::CallDirect {
                dst: resolve(instruction.a())?,
                callee_fn_idx: call.callee().0,
                arg_base: resolve(instruction.b())?,
                arg_count: call.argument_count(),
            })
        }
        Opcode::GetProperty => {
            if let Some(Some(cache)) = profile.get(pc as usize) {
                Ok(TemplateInstruction::GetPropShaped {
                    dst: resolve(instruction.a())?,
                    obj: resolve(instruction.b())?,
                    shape_id: cache.shape_id().0,
                    slot_index: cache.slot_index(),
                })
            } else {
                Err(TemplateCompileError::UnsupportedOpcode { pc, opcode: Opcode::GetProperty })
            }
        }
        Opcode::SetProperty => {
            if let Some(Some(cache)) = profile.get(pc as usize) {
                Ok(TemplateInstruction::SetPropShaped {
                    obj: resolve(instruction.a())?,
                    shape_id: cache.shape_id().0,
                    slot_index: cache.slot_index(),
                    src: resolve(instruction.b())?,
                })
            } else {
                Err(TemplateCompileError::UnsupportedOpcode { pc, opcode: Opcode::SetProperty })
            }
        }
        opcode => Err(TemplateCompileError::UnsupportedOpcode { pc, opcode }),
    }
}

fn resolve_target_pc(pc: u32, offset: i32) -> Option<u32> {
    let current = i64::from(pc);
    let target = current + 1 + i64::from(offset);
    u32::try_from(target).ok()
}

#[cfg(target_arch = "x86_64")]
fn emit_template_stencil_x86_64(
    program: &TemplateProgram,
) -> Result<CodeBuffer, TemplateEmitError> {
    use crate::arch::x64::{Assembler, Reg};

    #[derive(Debug, Clone, Copy)]
    enum BranchKind {
        Unconditional,
        Ge,
    }

    #[derive(Debug, Clone, Copy)]
    struct BranchPatch {
        source_offset: u32,
        target_pc: u32,
        kind: BranchKind,
    }

    let mut buf = CodeBuffer::new();
    let mut asm = Assembler::new(&mut buf);
    let mut patches = Vec::new();
    let mut pc_to_offset = std::collections::HashMap::new();

    // Mapping for registers within the JitContext provided by the runtime.
    // The context pointer is in rdi (Arg 0 per System V ABI).
    // We pin it to r15 to avoid clobbering by scratch work.
    let ctx_reg = Reg::R15;
    let local_base = crate::context::offsets::REGISTERS_BASE as u32;

    // Prologue: pin context pointer
    asm.mov_rr(ctx_reg, Reg::Rdi);

    for (pc, instruction) in program.instructions.iter().enumerate() {
        let pc = pc as u32;
        pc_to_offset.insert(pc, buf.len() as u32);

        match instruction {
            TemplateInstruction::LoadI32 { dst, imm } => {
                // dst = imm
                asm.mov_imm64(Reg::Rax, (*imm as u64) | 0x7FF8_0001_0000_0000);
                asm.mov_rm_u32(ctx_reg, local_base + u32::from(*dst) * 8, Reg::Rax);
            }
            TemplateInstruction::Move { dst, src } => {
                asm.mov_mr_u32(Reg::Rax, ctx_reg, local_base + u32::from(*src) * 8);
                asm.mov_rm_u32(ctx_reg, local_base + u32::from(*dst) * 8, Reg::Rax);
            }
            TemplateInstruction::AddI32 { dst, lhs, rhs } => {
                asm.mov_mr_u32(Reg::Rax, ctx_reg, local_base + u32::from(*lhs) * 8);
                asm.mov_mr_u32(Reg::Rcx, ctx_reg, local_base + u32::from(*rhs) * 8);
                asm.add_rr(Reg::Rax, Reg::Rcx);
                asm.extract_int32(Reg::Rax, Reg::Rax);
                asm.box_int32(Reg::Rax, Reg::Rax);
                asm.mov_rm_u32(ctx_reg, local_base + u32::from(*dst) * 8, Reg::Rax);
            }
            TemplateInstruction::LtI32 { dst, lhs, rhs } => {
                asm.mov_mr_u32(Reg::Rax, ctx_reg, local_base + u32::from(*lhs) * 8);
                asm.mov_mr_u32(Reg::Rcx, ctx_reg, local_base + u32::from(*rhs) * 8);
                asm.extract_int32(Reg::Rax, Reg::Rax);
                asm.extract_int32(Reg::Rcx, Reg::Rcx);
                // use cmp eax, ecx
                asm.buf.emit_u8(0x39); // cmp r/m32, r32
                asm.modrm_rr(Reg::Rcx, Reg::Rax);
                // setl al
                asm.buf.emit_u8(0x0F);
                asm.buf.emit_u8(0x9C);
                asm.modrm_rr(Reg::Rax, Reg::Rax);
                // box bool
                asm.mov_imm64(Reg::Rcx, 0x7FF8_0000_0000_0001); // TAG_BOOL
                asm.and_imm32(Reg::Rax, 1);
                asm.add_rr(Reg::Rax, Reg::Rcx);
                asm.mov_rm_u32(ctx_reg, local_base + u32::from(*dst) * 8, Reg::Rax);
            }
            TemplateInstruction::Jump { target_pc } => {
                patches.push(BranchPatch {
                    source_offset: buf.len() as u32,
                    target_pc: *target_pc,
                    kind: BranchKind::Unconditional,
                });
                // jmp rel32 placeholder
                asm.buf.emit_u8(0xE9);
                asm.buf.emit_u32_le(0);
            }
            TemplateInstruction::JumpIfFalse { cond, target_pc } => {
                asm.mov_mr_u32(Reg::Rax, ctx_reg, local_base + u32::from(*cond) * 8);
                // test rax, 1 (bool)
                asm.test_rr(Reg::Rax, Reg::Rax);
                asm.extract_int32(Reg::Rax, Reg::Rax);
                asm.test_rr(Reg::Rax, Reg::Rax);
                patches.push(BranchPatch {
                    source_offset: buf.len() as u32,
                    target_pc: *target_pc,
                    kind: BranchKind::Ge, // actually JZ if false
                });
                // jz rel32
                asm.buf.emit_u8(0x0F);
                asm.buf.emit_u8(0x84);
                asm.buf.emit_u32_le(0);
            }
            TemplateInstruction::Return { src } => {
                asm.mov_mr_u32(Reg::Rax, ctx_reg, local_base + u32::from(*src) * 8);
                asm.ret();
            }
            TemplateInstruction::SetPropShaped { obj, shape_id, slot_index, src } => {
                asm.mov_mr_u32(Reg::R10, ctx_reg, local_base + u32::from(*obj) * 8);
                // Check tag
                asm.check_object_tag(Reg::R10);
                
                // Extract handle: lower 32 bits
                asm.extract_int32(Reg::R11, Reg::R10);
                // slot_addr = heap_slots_base + handle * 32
                asm.mov_mr_u32(Reg::Rdx, ctx_reg, crate::context::offsets::HEAP_SLOTS_BASE as u32);
                asm.shl_ri(Reg::R11, 5);
                asm.add_rr(Reg::R11, Reg::Rdx);
                
                // Get JsObject pointer from Slot
                asm.mov_mr_u32(Reg::R12, Reg::R11, 0);

                // Shape check
                asm.mov_imm64(Reg::Rcx, *shape_id);
                asm.mov_mr_u32(Reg::Rax, Reg::R12, crate::context::offsets::js_object::SHAPE_ID as u32);
                // cmp rax, rcx
                asm.buf.emit_u8(0x48); asm.buf.emit_u8(0x39); asm.modrm_rr(Reg::Rcx, Reg::Rax);
                
                // Get Values base
                asm.mov_mr_u32(Reg::R13, Reg::R12, crate::context::offsets::js_object::VALUES_PTR as u32);
                // Offset = index * 24 + 8
                let val_offset = u64::from(*slot_index) * 24 + 8;
                asm.mov_imm64(Reg::R14, val_offset);
                asm.add_rr(Reg::R13, Reg::R14);

                // Load source value
                asm.mov_mr_u32(Reg::Rax, ctx_reg, local_base + u32::from(*src) * 8);
                // Store to object
                asm.mov_rm_u32(Reg::R13, 0, Reg::Rax);
                
                // Call write barrier helper
                asm.mov_imm64(Reg::R12, crate::runtime_helpers::otter_baseline_write_barrier as u64);
                asm.mov_rr(Reg::Rdi, ctx_reg); // Arg 0: JitContext
                asm.mov_rr(Reg::Rsi, Reg::R10); // Arg 1: obj_raw
                asm.mov_rr(Reg::Rdx, Reg::Rax); // Arg 2: src_raw
                asm.call_r(Reg::R12);
            }
            TemplateInstruction::GetPropShaped { dst, obj, shape_id, slot_index } => {
                asm.mov_mr_u32(Reg::R10, ctx_reg, local_base + u32::from(*obj) * 8);
                asm.check_object_tag(Reg::R10);
                asm.extract_int32(Reg::R11, Reg::R10);
                asm.mov_mr_u32(Reg::Rdx, ctx_reg, crate::context::offsets::HEAP_SLOTS_BASE as u32);
                asm.shl_ri(Reg::R11, 5);
                asm.add_rr(Reg::R11, Reg::Rdx);
                
                // Get JsObject pointer from Slot
                asm.mov_mr_u32(Reg::R12, Reg::R11, 0);

                // Shape check
                asm.mov_imm64(Reg::Rcx, *shape_id);
                asm.mov_mr_u32(Reg::Rax, Reg::R12, crate::context::offsets::js_object::SHAPE_ID as u32);
                asm.buf.emit_u8(0x48); asm.buf.emit_u8(0x39); asm.modrm_rr(Reg::Rcx, Reg::Rax);
                
                // Get Values base
                asm.mov_mr_u32(Reg::R13, Reg::R12, crate::context::offsets::js_object::VALUES_PTR as u32);
                // Offset = index * 24 + 8
                let val_offset = u64::from(*slot_index) * 24 + 8;
                asm.mov_imm64(Reg::R14, val_offset);
                asm.add_rr(Reg::R13, Reg::R14);

                // Load value
                asm.mov_mr_u32(Reg::Rax, Reg::R13, 0);
                asm.mov_rm_u32(ctx_reg, local_base + u32::from(*dst) * 8, Reg::Rax);
            }
        }
    }

    // Final entry PC mapping (no-op as we use HashMap)
    
    // Apply patches
    for patch in patches {
        let source_offset = patch.source_offset;
        let target_pc = patch.target_pc;
        let target_offset = *pc_to_offset.get(&target_pc).ok_or(TemplateEmitError::BranchTargetOutOfRange {
            source_offset,
            target_pc,
        })?;
        
        let relative = (target_offset as i32 - (source_offset as i32 + 5)) as u32; // rel32 after opcode+imm
        match patch.kind {
            BranchKind::Unconditional => {
                let offset = (source_offset + 1) as usize;
                buf.write_u32_le_at(offset, relative);
            }
            BranchKind::Ge => {
                let offset = (source_offset + 2) as usize; // skip 0x0F 0x84
                buf.write_u32_le_at(offset, relative);
            }
        }
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::bytecode::{Bytecode, BytecodeRegister, JumpOffset};
    use otter_vm::frame::FrameLayout;

    fn loop_function() -> Function {
        Function::with_bytecode(
            Some("baseline_loop"),
            FrameLayout::new(0, 0, 0, 5).expect("layout"),
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 0),
                Instruction::load_i32(BytecodeRegister::new(1), 0),
                Instruction::load_i32(BytecodeRegister::new(2), 10),
                Instruction::load_i32(BytecodeRegister::new(4), 1),
                Instruction::lt(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(2),
                ),
                Instruction::jump_if_false(BytecodeRegister::new(3), JumpOffset::new(3)),
                Instruction::add(
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
                Instruction::add(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(4),
                ),
                Instruction::jump(JumpOffset::new(-5)),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
        )
    }

    #[test]
    fn analyze_loop() {
        let func = loop_function();
        let program = analyze_template_candidate(&func, &[]).expect("analyze");
        assert_eq!(program.instructions.len(), 10);
        assert_eq!(program.loop_headers, vec![4]);
        assert!(matches!(
            program.instructions[5],
            TemplateInstruction::JumpIfFalse {
                cond: 3,
                target_pc: 9
            }
        ));
    }
}
