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
//!   host-pinned accumulator baseline code
//! ```

use otter_vm::bytecode::{InstructionIter, Opcode, Operand};
use otter_vm::feedback::{ArithmeticFeedback, FeedbackSlotId, FeedbackVector};
use otter_vm::module::{Function, FunctionIndex};

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
    /// Direct-call boundary compiled as an unconditional deopt. This
    /// keeps the surrounding hot prefix JIT-eligible while the
    /// interpreter executes the actual call sequence.
    CallDirect {
        callee: FunctionIndex,
        arg_base: u16,
        arg_count: u16,
    },
}

/// Comparison kind carried across `CompareAcc` → `JumpIfCompareFalse`.
/// The emitter uses this to pick the right host condition code.
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
    /// Absolute frame-slot offset of bytecode-visible register `r0`.
    /// Source-compiled functions reserve hidden slots (for `this`,
    /// future closure metadata, etc.) ahead of the user-visible window,
    /// so the emitter must translate bytecode registers through this
    /// base before touching the shared register file.
    pub user_visible_base: u16,
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
    /// Per-instruction int32-trust flag (same length as `instructions`).
    /// `true` means the instruction's persistent `ArithmeticFeedback`
    /// has stabilised at `Int32`, so the emitter may skip the tag guard
    /// on the instruction's RHS load. Produced by
    /// [`analyze_template_candidate_with_feedback`]; the profile-free
    /// variant leaves this all-`false`, preserving the guarded fast
    /// path.
    pub trust_int32: Vec<bool>,
    /// Slots ranked as best candidates for loop-local register pinning,
    /// top-first by total read + write frequency inside loop bodies.
    /// Only slots that appear in at least one loop are listed; pure
    /// straight-line code produces an empty list. Entries are unique
    /// and stable across equivalent programs (ties broken by slot id).
    ///
    /// Per-arch emitters pick a prefix of this list up to their
    /// callee-saved-register budget:
    /// - aarch64: up to 4 (pins into `x22..x25`).
    /// - x86_64: up to 2 (pins into `rbp` and `r15`).
    ///
    /// Populated by [`analyze_template_candidate`]; downstream
    /// feedback-aware analysis does not affect the ranking.
    pub pinning_candidates: Vec<u16>,
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
/// `Jump`, `JumpIfToBooleanFalse`, `CallDirect` (as a deopt boundary),
/// `Return`.
///
/// All other opcodes surface `UnsupportedOpcode` and prevent the
/// function from entering the v2 baseline path.
pub fn analyze_template_candidate(
    function: &Function,
) -> Result<TemplateProgram, TemplateCompileError> {
    analyze_template_candidate_with_feedback(function, None)
}

/// Feedback-aware analyzer variant.
///
/// Walks the same bytecode as [`analyze_template_candidate`] but
/// additionally consults the function's persistent [`FeedbackVector`]
/// (via the bytecode's sparse `FeedbackMap`) to populate
/// [`TemplateProgram::trust_int32`]. Instructions whose arithmetic
/// feedback has stabilised at [`ArithmeticFeedback::Int32`] after
/// warmup get their trust flag set so the emitter can drop the tag
/// guard on the RHS load.
///
/// Passing `feedback = None` is equivalent to
/// [`analyze_template_candidate`] and leaves every entry of
/// `trust_int32` as `false`.
pub fn analyze_template_candidate_with_feedback(
    function: &Function,
    feedback: Option<&FeedbackVector>,
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

    // Build the trust_int32 side table: one flag per emitted
    // instruction, derived from the feedback slot attached to the
    // instruction's byte-PC. A flag is `true` only when ALL of the
    // following hold:
    //
    //   1. A feedback slot is attached to this byte-PC (the source
    //      compiler only attaches slots to arithmetic ops, so this
    //      gates the whole check on "is arithmetic?").
    //   2. A `FeedbackVector` was supplied to the analyzer (this is the
    //      post-warmup recompile; the first compile has no feedback
    //      and keeps the guarded variant).
    //   3. The vector's slot has stabilised at
    //      `ArithmeticFeedback::Int32` — meaning every observation so
    //      far was int32. Any Number/BigInt/Any observation keeps the
    //      flag `false` so the emitter retains the guard.
    let bytecode_feedback_map = function.bytecode().feedback();
    let mut trust_int32 = vec![false; instructions.len()];
    if let Some(fv) = feedback {
        for (i, byte_pc) in byte_pcs.iter().enumerate() {
            if let Some(slot) = bytecode_feedback_map.get(*byte_pc)
                && let Some(ArithmeticFeedback::Int32) = fv.arithmetic(FeedbackSlotId(slot.0))
            {
                trust_int32[i] = true;
            }
        }
    }

    // Compute the function-wide pinning candidate ranking. Walk each
    // loop body once (loop ranges come from `loop_header_byte_pcs`
    // and their matching back-edges), count read + write references
    // per slot, rank by count with stable ties on slot id, and drop
    // any slot whose READ uses are not all trusted as int32.
    //
    // The trust-int32 filter is what gates pinning to the post-warmup
    // path: a cold compile leaves `trust_int32` all-`false`, so no
    // slot passes and `pinning_candidates` is empty — the emitter
    // then skips the entire pinning prologue and produces the same
    // stencil as the pre-M_JIT_C.3 baseline. A warm recompile with
    // stable `Int32` feedback on every READ of `s` / `i` / `n`
    // promotes those slots into the candidate list and the emitter
    // pins them into callee-saved registers for the life of the
    // function.
    let pinning_candidates = compute_pinning_candidates(
        &instructions,
        &byte_pcs,
        &loop_header_byte_pcs,
        &trust_int32,
    );

    Ok(TemplateProgram {
        function_name: function
            .name()
            .map(str::to_string)
            .unwrap_or_else(|| "<anonymous>".to_string()),
        user_visible_base: function.frame_layout().user_visible_start(),
        register_count: function.frame_layout().register_count(),
        instructions,
        byte_pcs,
        loop_header_byte_pcs,
        trust_int32,
        pinning_candidates,
    })
}

/// Loop-carried liveness pass: rank user-visible register slots by
/// total read + write frequency inside any loop body.
///
/// The result is a deterministic, unique list of slots ordered by
/// descending frequency, with ties broken by ascending slot id. The
/// list is truncated at [`MAX_PINNING_CANDIDATES`] so per-arch
/// emitters can always take a fixed-size prefix.
///
/// Loop bodies are bounded by `loop_header_byte_pcs` and the matching
/// back-edge: for each header, the back-edge is the nearest
/// `Jump`/`JumpIfAccFalse`/`JumpIfCompareFalse` whose `target_pc`
/// equals the header's byte-PC. Instructions with `byte_pcs[i]` in
/// the range `[header_pc, back_edge_pc]` count as inside that loop.
///
/// `trust_int32` is the per-instruction int32-trust flag (same shape
/// as [`TemplateProgram::trust_int32`]). A slot is retained as a
/// pinning candidate only when every READ reference to it inside
/// loop bodies has its `trust_int32` flag set. Writes are
/// unconditionally safe (the pinned reg mirrors the sign-extended
/// int32 acc), so they don't impose any trust requirement. Because a
/// cold compile has `trust_int32` all-false, no slot passes the
/// filter and the candidate list is empty — cold stencils emit no
/// pinning prologue. Warm recompiles with stable `Int32` feedback
/// promote the loop-carried slots into the candidate list.
fn compute_pinning_candidates(
    instructions: &[TemplateInstruction],
    byte_pcs: &[u32],
    loop_header_byte_pcs: &[u32],
    trust_int32: &[bool],
) -> Vec<u16> {
    if loop_header_byte_pcs.is_empty() || instructions.is_empty() {
        return Vec::new();
    }

    // Per-slot reference count across all loop bodies. A slot that
    // sits inside two nested loops is counted once per loop (matches
    // the natural "how hot is this slot in the actual execution
    // path" intuition). Slots that fail the trust-int32 filter never
    // appear in the map.
    let mut counts: std::collections::BTreeMap<u16, u32> = std::collections::BTreeMap::new();
    let mut disqualified: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();

    for &header_pc in loop_header_byte_pcs {
        let Some(back_edge_idx) = find_back_edge_index(instructions, byte_pcs, header_pc) else {
            continue;
        };
        let back_edge_pc = byte_pcs[back_edge_idx];
        for (i, instr) in instructions.iter().enumerate() {
            let instr_pc = byte_pcs[i];
            if instr_pc < header_pc || instr_pc > back_edge_pc {
                continue;
            }
            let Some(slot) = slot_referenced(instr) else {
                continue;
            };
            // Read uses need trust-int32 so the prologue can load the
            // pinned slot without a tag guard and in-loop reads can
            // use the pinned reg directly. Writes don't read the
            // slot, so they never fail this check.
            if is_read_reference(instr) && !trust_int32[i] {
                disqualified.insert(slot);
                continue;
            }
            *counts.entry(slot).or_default() += 1;
        }
    }

    for slot in disqualified {
        counts.remove(&slot);
    }

    // Rank: highest count first; within a count, lowest slot id first
    // (so the ordering is reproducible between runs with the same
    // bytecode).
    let mut ranked: Vec<(u16, u32)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(MAX_PINNING_CANDIDATES);
    ranked.into_iter().map(|(slot, _)| slot).collect()
}

/// `true` when the instruction READS its slot operand — it requires
/// `trust_int32` to elide the load's tag guard. Writes (`Star`) don't
/// read the slot before clobbering it, so they need no such
/// guarantee.
fn is_read_reference(instr: &TemplateInstruction) -> bool {
    matches!(
        instr,
        TemplateInstruction::Ldar { .. }
            | TemplateInstruction::AddAcc { .. }
            | TemplateInstruction::SubAcc { .. }
            | TemplateInstruction::MulAcc { .. }
            | TemplateInstruction::BitOrAcc { .. }
            | TemplateInstruction::CompareAcc { .. }
            | TemplateInstruction::Mov { .. }
    )
}

/// Find the instruction index of the first back-edge whose
/// `target_pc` matches `header_pc`. Returns `None` if no such
/// instruction exists — that should not happen in well-formed
/// programs, since `loop_header_byte_pcs` was populated from exactly
/// these back-edges during analysis, but the emitter handles the
/// missing case gracefully by simply leaving the loop out of the
/// liveness accumulation.
fn find_back_edge_index(
    instructions: &[TemplateInstruction],
    byte_pcs: &[u32],
    header_pc: u32,
) -> Option<usize> {
    instructions.iter().enumerate().find_map(|(idx, instr)| {
        let instr_pc = byte_pcs[idx];
        if instr_pc < header_pc {
            return None;
        }
        match instr {
            TemplateInstruction::Jump { target_pc }
            | TemplateInstruction::JumpIfAccFalse { target_pc }
            | TemplateInstruction::JumpIfCompareFalse { target_pc, .. }
                if *target_pc == header_pc =>
            {
                Some(idx)
            }
            _ => None,
        }
    })
}

/// Extract the user-visible register slot an instruction references
/// as an operand, if any. Returns `None` for ops that don't touch a
/// slot (immediate arithmetic, branches, `Return`, tag-constant
/// loads, etc.).
///
/// Slots referenced more than once by the same instruction (currently
/// none — every op that references a slot references exactly one)
/// would be undercounted, but that isn't a correctness issue: the
/// ranking is a heuristic.
fn slot_referenced(instr: &TemplateInstruction) -> Option<u16> {
    match instr {
        TemplateInstruction::Ldar { reg }
        | TemplateInstruction::Star { reg }
        | TemplateInstruction::AddAcc { rhs: reg }
        | TemplateInstruction::SubAcc { rhs: reg }
        | TemplateInstruction::MulAcc { rhs: reg }
        | TemplateInstruction::BitOrAcc { rhs: reg }
        | TemplateInstruction::CompareAcc { rhs: reg, .. } => Some(*reg),
        TemplateInstruction::Mov { dst: _, src } => Some(*src),
        _ => None,
    }
}

/// Maximum number of pinning candidates [`compute_pinning_candidates`]
/// reports. Set slightly above the largest per-arch budget so future
/// emitter growth (e.g., enabling `x26/x27` or adding `rbx`-class
/// slots on x86_64) can grab more without re-running the analyzer.
pub const MAX_PINNING_CANDIDATES: usize = 6;

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
        Opcode::CallDirect => {
            let callee = idx_u32(&r.operands, 0, bp)?;
            let (arg_base, arg_count) = reg_list_u16(&r.operands, 1, bp)?;
            Ok(TemplateInstruction::CallDirect {
                callee: FunctionIndex(callee),
                arg_base,
                arg_count,
            })
        }
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

fn idx_u32(ops: &[Operand], pos: usize, byte_pc: u32) -> Result<u32, TemplateCompileError> {
    match ops.get(pos) {
        Some(Operand::Idx(v)) => Ok(*v),
        _ => Err(TemplateCompileError::OperandKindMismatch {
            byte_pc,
            expected: "Idx",
        }),
    }
}

fn reg_list_u16(
    ops: &[Operand],
    pos: usize,
    byte_pc: u32,
) -> Result<(u16, u16), TemplateCompileError> {
    match ops.get(pos) {
        Some(Operand::RegList { base, count }) => {
            let base =
                u16::try_from(*base).map_err(|_| TemplateCompileError::OperandKindMismatch {
                    byte_pc,
                    expected: "RegList base fits in u16",
                })?;
            let count =
                u16::try_from(*count).map_err(|_| TemplateCompileError::OperandKindMismatch {
                    byte_pc,
                    expected: "RegList count fits in u16",
                })?;
            Ok((base, count))
        }
        _ => Err(TemplateCompileError::OperandKindMismatch {
            byte_pc,
            expected: "RegList",
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
// Phase 4.2 emitter: host-arch stencil generation for a TemplateProgram.
// ---------------------------------------------------------------------------

const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;

/// One on-stack-replacement entry point produced by the emitter.
///
/// `byte_pc` is the bytecode-space PC of a loop header that was deemed
/// OSR-eligible (its first body op unconditionally overwrites the
/// accumulator register, so the trampoline's raw-bit load into x21/r13
/// is harmless). `native_offset` is the byte offset inside the emitted
/// code buffer of the trampoline that pins the JIT registers, rehydrates
/// the accumulator from `JitContext::accumulator_raw`, and jumps into
/// the loop body at that PC.
///
/// The interpreter looks these up via [`crate::code_cache::osr_native_offset`]
/// when a back-edge crosses the hotness budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsrEntry {
    /// Bytecode PC of the loop header this trampoline targets.
    pub byte_pc: u32,
    /// Byte offset of the trampoline inside the emitted code buffer.
    pub native_offset: u32,
}

/// Output of [`emit_template_stencil`]: the executable code buffer plus
/// the per-loop-header OSR entry table.
///
/// The OSR table is empty when no loop header in the function passes the
/// safety filter (`is_osr_safe_first_op`); the stencil still functions
/// for normal-call entry in that case.
#[derive(Debug)]
pub struct EmittedStencil {
    /// Raw machine code; ownership transferred into `compile_code_buffer`.
    pub code: crate::arch::CodeBuffer,
    /// `(byte_pc, native_offset)` for each emitted OSR entry trampoline.
    /// Sorted by `byte_pc` so the cache lookup can do an O(log N) probe.
    pub osr_entries: Vec<OsrEntry>,
}

/// Returns `true` if a loop header beginning with `op` can be safely
/// entered via OSR.
///
/// The OSR trampoline loads `JitContext::accumulator_raw` (the raw spill
/// from the interpreter's last accumulator value) into x21/r13 before
/// jumping into the body. That's safe whenever the first op of the loop
/// header overwrites x21/r13 without reading it, since the body never
/// observes the OSR-loaded raw bits in any acc-state-sensitive way.
///
/// `Star` and the arithmetic ops are excluded — they consume x21 in an
/// acc-state-dependent way (`box_int32` vs raw `str`, int32 ALU vs
/// bailout). Allowing OSR there would require either a tagged
/// rehydration path or an analyzer-driven OSR map, both of which are
/// out of scope for M_JIT_C.1.
fn is_osr_safe_first_op(op: &TemplateInstruction) -> bool {
    matches!(
        op,
        TemplateInstruction::Ldar { .. }
            | TemplateInstruction::LdaI32 { .. }
            | TemplateInstruction::LdaTagConst { .. }
            | TemplateInstruction::LdaThis
            | TemplateInstruction::LdaCurrentClosure
            | TemplateInstruction::Mov { .. }
    )
}

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

/// Emit a Phase 4.5b template-baseline stencil for a [`TemplateProgram`].
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
) -> Result<EmittedStencil, TemplateEmitError> {
    #[cfg(target_arch = "aarch64")]
    {
        emit_template_stencil_aarch64(program)
    }
    #[cfg(target_arch = "x86_64")]
    {
        emit_template_stencil_x64(program)
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = program;
        Err(TemplateEmitError::UnsupportedHostArch(
            std::env::consts::ARCH,
        ))
    }
}

/// Build the sorted OSR entry list for a freshly emitted body.
///
/// Walks the program's loop headers, drops any that don't pass the
/// `is_osr_safe_first_op` filter or that don't appear in `body_offsets`
/// (defensive: the latter shouldn't happen because every analyzed
/// instruction emits at least one byte), and produces a `Vec<OsrEntry>`
/// sorted by `byte_pc` for the cache lookup.
fn collect_osr_candidates(
    program: &TemplateProgram,
    body_offsets: &[(u32, u32)],
) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for &header_pc in &program.loop_header_byte_pcs {
        let Some(idx) = program.byte_pcs.iter().position(|pc| *pc == header_pc) else {
            continue;
        };
        let Some(op) = program.instructions.get(idx) else {
            continue;
        };
        if !is_osr_safe_first_op(op) {
            continue;
        }
        let Some(&(_, body_off)) = body_offsets.iter().find(|(pc, _)| *pc == header_pc) else {
            continue;
        };
        out.push((header_pc, body_off));
    }
    out.sort_by_key(|(pc, _)| *pc);
    out
}

/// aarch64 callee-saved registers the pinning pass is allowed to claim
/// for loop-local slots. Ordered — the i-th entry in
/// `TemplateProgram::pinning_candidates` (truncated to
/// [`AARCH64_PINNED_REGS`]'s length) binds to `AARCH64_PINNED_REGS[i]`.
///
/// We intentionally stop at four regs (x22..x25) so the prologue can
/// save them with at most two `stp` pairs and the bailout spill loop
/// stays compact. The analyzer's [`MAX_PINNING_CANDIDATES = 6`] ceiling
/// leaves headroom to grow this array once per-register-pair cost
/// analysis picks up the remaining x26/x27.
#[cfg(target_arch = "aarch64")]
const AARCH64_PINNED_REGS: [crate::arch::aarch64::Reg; 4] = [
    crate::arch::aarch64::Reg::X22,
    crate::arch::aarch64::Reg::X23,
    crate::arch::aarch64::Reg::X24,
    crate::arch::aarch64::Reg::X25,
];

/// Push the claimed callee-saved pinning regs as 16-byte pairs. Odd
/// pin counts pad the last pair with `xzr`. Matching pops live in
/// [`pop_pinned_pairs_aarch64`].
#[cfg(target_arch = "aarch64")]
fn push_pinned_pairs_aarch64(
    asm: &mut crate::arch::aarch64::Assembler,
    pinned: &[(u16, crate::arch::aarch64::Reg)],
) {
    use crate::arch::aarch64::Reg;
    let mut iter = pinned.iter();
    while let Some((_, first)) = iter.next() {
        let second = iter.next().map(|(_, r)| *r).unwrap_or(Reg::Xzr);
        asm.stp_pair_push(*first, second);
    }
}

/// Mirror [`push_pinned_pairs_aarch64`] for the epilogue: pop pairs
/// in reverse order so the stack layout matches. The `xzr` slot in
/// the odd-count case is loaded into xzr (a legal discard).
#[cfg(target_arch = "aarch64")]
fn pop_pinned_pairs_aarch64(
    asm: &mut crate::arch::aarch64::Assembler,
    pinned: &[(u16, crate::arch::aarch64::Reg)],
) {
    use crate::arch::aarch64::Reg;
    // Walk pairs from the outer-most push inward. For an odd count,
    // the last pair pushed was `(last_reg, xzr)`; we pop it first.
    let mut pairs: Vec<(Reg, Reg)> = Vec::with_capacity(pinned.len().div_ceil(2));
    let mut iter = pinned.iter();
    while let Some((_, first)) = iter.next() {
        let second = iter.next().map(|(_, r)| *r).unwrap_or(Reg::Xzr);
        pairs.push((*first, second));
    }
    for (a, b) in pairs.into_iter().rev() {
        asm.ldp_pair_pop(a, b);
    }
}

/// x86_64 SysV callee-saved registers the pinning pass is allowed to
/// claim. `rbx`/`r12`/`r13`/`r14` are already pinned to the JIT's
/// own execution state (`JitContext*` / `registers_base` /
/// `accumulator` / `TAG_INT32`), leaving `rbp` and `r15` as the
/// two free callee-saved slots. `rbp` is the ABI frame pointer, but
/// the stencil never calls into external code that expects a frame
/// chain, so reclaiming it for pinning is safe here.
///
/// Ordering is deterministic so two compiles of the same bytecode
/// produce identical stencils; `r15` goes first because it has a
/// shorter REX-free encoding for most `mov`/`add` variants.
#[cfg(target_arch = "x86_64")]
const X64_PINNED_REGS: [crate::arch::x64::Reg; 2] =
    [crate::arch::x64::Reg::R15, crate::arch::x64::Reg::Rbp];

#[cfg(target_arch = "aarch64")]
fn emit_template_stencil_aarch64(
    program: &TemplateProgram,
) -> Result<EmittedStencil, TemplateEmitError> {
    use crate::arch::CodeBuffer;
    use crate::arch::aarch64::{Assembler, Cond, Reg};

    fn slot_offset(program: &TemplateProgram, slot: u16) -> Result<u32, TemplateEmitError> {
        let absolute_slot = program
            .user_visible_base
            .checked_add(slot)
            .ok_or(TemplateEmitError::RegisterSlotOutOfRange { slot })?;
        let byte_offset = u32::from(absolute_slot) * 8;
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
    ///
    /// When `trust_int32` is `true`, the guard and its bailout patch
    /// are elided: only `ldr` + `sxtw` remain. The caller (the
    /// feedback-driven analyzer) is responsible for ensuring the slot
    /// really does hold an int32 at runtime, either because
    /// [`ArithmeticFeedback::Int32`] has stabilised on the associated
    /// PC across many observations or because an upstream guard
    /// already validated the operand.
    fn load_int32_guarded(
        asm: &mut Assembler,
        dst: Reg,
        slot_off: u32,
        byte_pc: u32,
        acc_state_at_guard: AccState,
        bailout_patches: &mut Vec<BailoutPatch>,
        trust_int32: bool,
    ) {
        asm.ldr_u64_imm(dst, Reg::X9, slot_off);
        if !trust_int32 {
            asm.check_int32_tag_fast(dst, Reg::X20);
            let bp = asm.b_cond_placeholder(Cond::Ne);
            bailout_patches.push(BailoutPatch {
                source_offset: bp,
                byte_pc,
                reason: crate::BailoutReason::TypeGuardFailed as u32,
                acc_state: acc_state_at_guard,
            });
        }
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

    // ---- Pinning setup (M_JIT_C.3) ----
    //
    // Claim the first `AARCH64_PINNED_REGS.len()` candidates from the
    // analyzer's ranking (at most 4 on aarch64) and bind each to a
    // callee-saved register. `pinned` is the emitter's authoritative
    // slot→reg map; `reg_for_slot` walks it. An empty list disables
    // pinning end-to-end (cold compiles fall into this path).
    let pinned: Vec<(u16, Reg)> = program
        .pinning_candidates
        .iter()
        .take(AARCH64_PINNED_REGS.len())
        .enumerate()
        .map(|(i, &slot)| (slot, AARCH64_PINNED_REGS[i]))
        .collect();
    let reg_for_slot =
        |slot: u16| -> Option<Reg> { pinned.iter().find(|(s, _)| *s == slot).map(|(_, r)| *r) };

    // Prologue: 32-byte frame saving x19 + lr + x20. Same shape as v1
    // so the call-site ABI stays identical.
    asm.push_x19_lr_32();
    asm.str_x20_at_sp16();
    // Save the claimed callee-saved pinning regs (up to 4) as paired
    // 16-byte pushes. Odd counts pad with xzr so SP stays 16-aligned
    // and the epilogue's pops mirror pushes exactly.
    push_pinned_pairs_aarch64(&mut asm, &pinned);
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

    // Load each pinned slot into its claimed register, sign-extending
    // the low 32 bits. The analyzer only promotes slots whose READ
    // references are all `trust_int32` — the feedback lattice
    // guarantees (probabilistically) that the slot holds an int32 at
    // every observed call — so no tag guard runs here. A non-int32
    // value slipped through would propagate as a silently-corrupted
    // int32, matching the correctness envelope of M_JIT_C.2's elided
    // per-op guards; parameter-entry guards are a future refinement.
    for (slot, reg) in &pinned {
        asm.ldr_u64_imm(*reg, Reg::X9, slot_offset(program, *slot)?);
        asm.sxtw(*reg, *reg);
    }

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
                // Pinned slot: keep the pinned reg in sync with the
                // accumulator. Because the analyzer only pins slots
                // whose READ uses are all `trust_int32` and because
                // Star's `acc_state == Int32` invariant is the only
                // state the source compiler produces (every `Star`
                // is preceded by arithmetic or an int32 `Ldar`), we
                // can simply copy x21 into the pinned reg — x21 is
                // already sign-extended int32.
                if let Some(pr) = reg_for_slot(*reg) {
                    asm.mov_rr(pr, Reg::X21);
                } else {
                    store_accumulator(&mut asm, acc_state, slot_offset(program, *reg)?);
                }
                // Star doesn't touch x21.
            }
            TemplateInstruction::Ldar { reg } => {
                // Pinned slot: read directly out of the pinned reg.
                // The pinned reg already holds the sign-extended
                // int32 payload, so the accumulator moves straight
                // to `AccState::Int32` without a tag guard.
                if let Some(pr) = reg_for_slot(*reg) {
                    asm.mov_rr(Reg::X21, pr);
                } else {
                    // Unpinned path: the guard fires AFTER ldr has
                    // clobbered x21 with raw slot bits — so at the
                    // bailout point x21 holds raw (not yet sxtw'd).
                    // Spill as Raw. The existing M_JIT_C.2
                    // `trust_int32[i]` plumbing still eliminates the
                    // tag-guard insns when feedback has stabilised.
                    load_int32_guarded(
                        &mut asm,
                        Reg::X21,
                        slot_offset(program, *reg)?,
                        byte_pc,
                        AccState::Raw,
                        &mut bailout_patches,
                        program.trust_int32[i],
                    );
                }
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
                    // Source of the RHS operand: the pinned reg for
                    // pinned slots (no load, no guard), otherwise the
                    // usual guarded load into the x10 scratch.
                    let rhs_reg = if let Some(pr) = reg_for_slot(*rhs) {
                        pr
                    } else {
                        load_int32_guarded(
                            &mut asm,
                            Reg::X10,
                            slot_offset(program, *rhs)?,
                            byte_pc,
                            acc_state,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                        Reg::X10
                    };
                    asm.add_rrr(Reg::X21, Reg::X21, rhs_reg);
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
                    let rhs_reg = if let Some(pr) = reg_for_slot(*rhs) {
                        pr
                    } else {
                        load_int32_guarded(
                            &mut asm,
                            Reg::X10,
                            slot_offset(program, *rhs)?,
                            byte_pc,
                            acc_state,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                        Reg::X10
                    };
                    asm.sub_rrr(Reg::X21, Reg::X21, rhs_reg);
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
                    let rhs_reg = if let Some(pr) = reg_for_slot(*rhs) {
                        pr
                    } else {
                        load_int32_guarded(
                            &mut asm,
                            Reg::X10,
                            slot_offset(program, *rhs)?,
                            byte_pc,
                            acc_state,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                        Reg::X10
                    };
                    asm.mul_rrr(Reg::X21, Reg::X21, rhs_reg);
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
                    let rhs_reg = if let Some(pr) = reg_for_slot(*rhs) {
                        pr
                    } else {
                        load_int32_guarded(
                            &mut asm,
                            Reg::X10,
                            slot_offset(program, *rhs)?,
                            byte_pc,
                            acc_state,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                        Reg::X10
                    };
                    asm.orr_rrr(Reg::X21, Reg::X21, rhs_reg);
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
                    let rhs_reg = if let Some(pr) = reg_for_slot(*rhs) {
                        pr
                    } else {
                        load_int32_guarded(
                            &mut asm,
                            Reg::X10,
                            slot_offset(program, *rhs)?,
                            byte_pc,
                            acc_state,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                        Reg::X10
                    };
                    asm.cmp_rr(Reg::X21, rhs_reg);
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
                // Pop pinned regs first — they sit at a lower SP than
                // the base x19/lr/x20 frame. Return doesn't spill
                // pinned values back to memory: the activation is
                // destroyed on return, and any v2 caller reads its
                // own register window (not the callee's).
                pop_pinned_pairs_aarch64(&mut asm, &pinned);
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
                // Pinned-aware: use the pinned reg for either side
                // when it's the source or destination; otherwise load
                // via x10 scratch as before.
                let src_pr = reg_for_slot(*src);
                let dst_pr = reg_for_slot(*dst);
                match (src_pr, dst_pr) {
                    (Some(sp), Some(dp)) => {
                        asm.mov_rr(dp, sp);
                    }
                    (Some(sp), None) => {
                        asm.box_int32(Reg::X10, sp);
                        asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(program, *dst)?);
                    }
                    (None, Some(dp)) => {
                        // Source is memory. Load NaN-boxed value,
                        // keep the guard unless the analyzer already
                        // promised int32 at this PC — we still need
                        // the sign-extension for the pinned reg.
                        load_int32_guarded(
                            &mut asm,
                            dp,
                            slot_offset(program, *src)?,
                            byte_pc,
                            AccState::Raw,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                    }
                    (None, None) => {
                        asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(program, *src)?);
                        asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(program, *dst)?);
                    }
                }
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
            TemplateInstruction::CallDirect {
                callee: _,
                arg_base: _,
                arg_count: _,
            } => {
                emit_unconditional_bailout(
                    &mut asm,
                    byte_pc,
                    crate::BailoutReason::Unsupported as u32,
                    acc_state,
                    &mut bailout_patches,
                );
            }
        }
        acc_states.push(acc_state);
        i += 1;
    }

    // ----- Common bailout epilogue -----
    //
    // Per-site pads branch here AFTER populating: x10 = byte_pc,
    // x11 = reason, and spilling x21 into ctx.accumulator_raw. This
    // block writes the low-32-bit pc/reason fields, spills every
    // pinned register back to its slot so the interpreter's resume
    // sees a coherent frame, unwinds the prologue, and returns
    // BAILOUT_SENTINEL in x0.
    let bailout_common = asm.position();
    // Spill each pinned reg back as a NaN-boxed int32. The pinned
    // regs hold sign-extended int32 by emitter invariant, so
    // `box_int32(x12, pr)` produces the correctly-tagged memory value.
    for (slot, reg) in &pinned {
        asm.box_int32(Reg::X12, *reg);
        asm.str_u64_imm(Reg::X12, Reg::X9, slot_offset(program, *slot)?);
    }
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
    pop_pinned_pairs_aarch64(&mut asm, &pinned);
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

    // ----- Per-loop-header OSR trampolines -----
    //
    // For each loop header that passes the safety filter, emit a small
    // trampoline that mirrors the main prologue (so the body's epilogue
    // tears down the same frame), reloads the spilled accumulator from
    // `JitContext::accumulator_raw` into x21, and unconditionally
    // branches into the body at the loop header's emitted offset.
    //
    // The trampoline's tail branch uses `b_placeholder` and is patched
    // alongside the bailout / branch patches below, since the existing
    // patching machinery already operates on the raw `CodeBuffer`.
    let osr_candidates = collect_osr_candidates(program, &byte_pc_to_emit);
    let mut osr_entries: Vec<OsrEntry> = Vec::with_capacity(osr_candidates.len());
    let mut osr_tail_patches: Vec<(u32 /* src */, u32 /* target */)> =
        Vec::with_capacity(osr_candidates.len());
    for (header_pc, body_off) in osr_candidates {
        let osr_entry_offset = asm.position();
        // Mirror the main prologue so the body's epilogue (`pop
        // pinned + ldr x20 + pop x19/lr + ret`) restores the same
        // callee-saved state we saved here.
        asm.push_x19_lr_32();
        asm.str_x20_at_sp16();
        push_pinned_pairs_aarch64(&mut asm, &pinned);
        asm.mov_rr(Reg::X19, Reg::X0);
        asm.ldr_u64_imm(Reg::X9, Reg::X19, 0);
        asm.mov_imm64(Reg::X20, TAG_INT32);
        // Rehydrate the accumulator from the interpreter's spill slot.
        // Safe for the body's first op because `is_osr_safe_first_op`
        // restricted entry to ops that overwrite x21 without reading it.
        asm.ldr_u64_imm(
            Reg::X21,
            Reg::X19,
            crate::context::offsets::ACCUMULATOR_RAW as u32,
        );
        // Rehydrate pinned regs from their slots. Same reasoning as
        // the main prologue: no guard because pinning is restricted
        // to slots with `trust_int32 == true` everywhere they're
        // read. The interpreter's activation slots hold the latest
        // values (the back-edge we're entering from just executed in
        // the interpreter, which syncs writes to memory).
        for (slot, reg) in &pinned {
            asm.ldr_u64_imm(*reg, Reg::X9, slot_offset(program, *slot)?);
            asm.sxtw(*reg, *reg);
        }
        let src = asm.b_placeholder();
        osr_tail_patches.push((src, body_off));
        osr_entries.push(OsrEntry {
            byte_pc: header_pc,
            native_offset: osr_entry_offset,
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

    // Patch each OSR trampoline's trailing `b body_off`. Both endpoints
    // were known when we emitted the trampoline; the patching loop runs
    // after the asm binding is dropped because the existing pad/branch
    // patches are structured the same way.
    for &(src, target) in &osr_tail_patches {
        let delta = i64::from(target) - i64::from(src);
        if delta % 4 != 0 || !(i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&delta) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: src,
                target_byte_pc: target,
            });
        }
        let imm26 = ((delta / 4) as i32 as u32) & 0x03FF_FFFF;
        if !buf.patch_u32_le(src, 0x1400_0000 | imm26) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: src,
                target_byte_pc: target,
            });
        }
    }

    // acc_states is used transitively by the emit loop; keep the
    // declaration alive for downstream (and to prevent `unused_mut`
    // lints from firing when more analyses consume it).
    let _ = acc_states;

    Ok(EmittedStencil {
        code: buf,
        osr_entries,
    })
}

#[cfg(target_arch = "x86_64")]
fn emit_template_stencil_x64(
    program: &TemplateProgram,
) -> Result<EmittedStencil, TemplateEmitError> {
    use crate::arch::CodeBuffer;
    use crate::arch::x64::{Assembler, Cond, Reg};

    fn slot_offset(program: &TemplateProgram, slot: u16) -> Result<u32, TemplateEmitError> {
        let absolute_slot = program
            .user_visible_base
            .checked_add(slot)
            .ok_or(TemplateEmitError::RegisterSlotOutOfRange { slot })?;
        let byte_offset = u32::from(absolute_slot) * 8;
        if byte_offset > u32::MAX - 8 {
            return Err(TemplateEmitError::RegisterSlotOutOfRange { slot });
        }
        Ok(byte_offset)
    }

    fn load_int32_guarded(
        asm: &mut Assembler,
        dst: Reg,
        slot_off: u32,
        byte_pc: u32,
        acc_state_at_guard: AccState,
        bailout_patches: &mut Vec<BailoutPatch>,
        trust_int32: bool,
    ) {
        asm.mov_mr_u32(dst, Reg::R12, slot_off);
        if !trust_int32 {
            asm.check_int32_tag_fast(dst, Reg::Rax, Reg::R14);
            let bp = asm.b_cond_placeholder(Cond::Ne);
            bailout_patches.push(BailoutPatch {
                source_offset: bp,
                byte_pc,
                reason: crate::BailoutReason::TypeGuardFailed as u32,
                acc_state: acc_state_at_guard,
            });
        }
        asm.sxtw(dst, dst);
    }

    fn store_accumulator(asm: &mut Assembler, state: AccState, slot_off: u32) {
        match state {
            AccState::Int32 => {
                asm.box_int32(Reg::R10, Reg::R13);
                asm.mov_rm_u32(Reg::R12, slot_off, Reg::R10);
            }
            AccState::Raw => {
                asm.mov_rm_u32(Reg::R12, slot_off, Reg::R13);
            }
        }
    }

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

    #[derive(Debug, Clone, Copy)]
    struct BranchPatch {
        source_offset: u32,
        target_byte_pc: u32,
        cond: Option<Cond>,
    }

    #[derive(Debug, Clone, Copy)]
    struct BailoutPatch {
        source_offset: u32,
        byte_pc: u32,
        reason: u32,
        acc_state: AccState,
    }

    let mut buf = CodeBuffer::new();
    let mut asm = Assembler::new(&mut buf);

    // ---- Pinning setup (M_JIT_C.3) ----
    //
    // Mirror the aarch64 path: claim the first
    // `X64_PINNED_REGS.len()` candidates (at most 2 on x86_64 SysV)
    // and bind them to callee-saved registers. `reg_for_slot` below
    // acts as the emitter's slot→reg map; an empty `pinned` list
    // disables pinning end-to-end.
    let pinned: Vec<(u16, Reg)> = program
        .pinning_candidates
        .iter()
        .take(X64_PINNED_REGS.len())
        .enumerate()
        .map(|(i, &slot)| (slot, X64_PINNED_REGS[i]))
        .collect();
    let reg_for_slot =
        |slot: u16| -> Option<Reg> { pinned.iter().find(|(s, _)| *s == slot).map(|(_, r)| *r) };

    // SysV x86_64 ABI:
    //   rdi = JitContext* on entry
    //   rbx = pinned JitContext*
    //   r12 = pinned registers_base
    //   r13 = pinned accumulator
    //   r14 = pinned TAG_INT32
    //   r10/r11/rax = scratch
    //   r15, rbp = loop-local pinning slots (when `pinned` is non-empty)
    asm.push_callee_saved();
    // Save the claimed pinning regs as individual pushes (SysV
    // requires we preserve the caller's rbp / r15 across the call).
    for (_, reg) in &pinned {
        asm.push(*reg);
    }
    asm.mov_rr(Reg::Rbx, Reg::Rdi);
    asm.mov_mr_u32(
        Reg::R12,
        Reg::Rbx,
        crate::context::offsets::REGISTERS_BASE as u32,
    );
    asm.mov_imm64(Reg::R14, TAG_INT32);
    asm.mov_imm64(Reg::R13, 0);
    // Load each pinned slot into its claimed register. Same
    // correctness envelope as aarch64: the analyzer filters
    // candidates by `trust_int32`, so the feedback lattice has
    // observed int32 at every read of each pinned slot.
    for (slot, reg) in &pinned {
        asm.mov_mr_u32(*reg, Reg::R12, slot_offset(program, *slot)?);
        asm.sxtw(*reg, *reg);
    }

    let mut branch_patches: Vec<BranchPatch> = Vec::new();
    let mut bailout_patches: Vec<BailoutPatch> = Vec::new();
    let mut byte_pc_to_emit: Vec<(u32, u32)> = Vec::with_capacity(program.instructions.len());
    let mut acc_states: Vec<AccState> = Vec::with_capacity(program.instructions.len());
    let mut acc_state = AccState::Int32;

    let n = program.instructions.len();
    let mut i = 0;
    while i < n {
        let byte_pc = program.byte_pcs[i];
        byte_pc_to_emit.push((byte_pc, asm.position()));

        match &program.instructions[i] {
            TemplateInstruction::LdaI32 { imm } => {
                asm.mov_imm64(Reg::R13, *imm as i64 as u64);
                acc_state = AccState::Int32;
            }
            TemplateInstruction::Star { reg } => {
                if let Some(pr) = reg_for_slot(*reg) {
                    asm.mov_rr(pr, Reg::R13);
                } else {
                    store_accumulator(&mut asm, acc_state, slot_offset(program, *reg)?);
                }
            }
            TemplateInstruction::Ldar { reg } => {
                if let Some(pr) = reg_for_slot(*reg) {
                    asm.mov_rr(Reg::R13, pr);
                } else {
                    load_int32_guarded(
                        &mut asm,
                        Reg::R13,
                        slot_offset(program, *reg)?,
                        byte_pc,
                        AccState::Raw,
                        &mut bailout_patches,
                        program.trust_int32[i],
                    );
                }
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
                    let rhs_reg = if let Some(pr) = reg_for_slot(*rhs) {
                        pr
                    } else {
                        load_int32_guarded(
                            &mut asm,
                            Reg::R10,
                            slot_offset(program, *rhs)?,
                            byte_pc,
                            acc_state,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                        Reg::R10
                    };
                    asm.add_rrr(Reg::R13, Reg::R13, rhs_reg);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    let rhs_reg = if let Some(pr) = reg_for_slot(*rhs) {
                        pr
                    } else {
                        load_int32_guarded(
                            &mut asm,
                            Reg::R10,
                            slot_offset(program, *rhs)?,
                            byte_pc,
                            acc_state,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                        Reg::R10
                    };
                    asm.sub_rrr(Reg::R13, Reg::R13, rhs_reg);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    let rhs_reg = if let Some(pr) = reg_for_slot(*rhs) {
                        pr
                    } else {
                        load_int32_guarded(
                            &mut asm,
                            Reg::R10,
                            slot_offset(program, *rhs)?,
                            byte_pc,
                            acc_state,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                        Reg::R10
                    };
                    asm.mul_rrr(Reg::R13, Reg::R13, rhs_reg);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    let rhs_reg = if let Some(pr) = reg_for_slot(*rhs) {
                        pr
                    } else {
                        load_int32_guarded(
                            &mut asm,
                            Reg::R10,
                            slot_offset(program, *rhs)?,
                            byte_pc,
                            acc_state,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                        Reg::R10
                    };
                    asm.orr_rrr(Reg::R13, Reg::R13, rhs_reg);
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
                    asm.mov_imm64(Reg::R10, *imm as i64 as u64);
                    asm.add_rrr(Reg::R13, Reg::R13, Reg::R10);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    asm.mov_imm64(Reg::R10, *imm as i64 as u64);
                    asm.sub_rrr(Reg::R13, Reg::R13, Reg::R10);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    asm.mov_imm64(Reg::R10, *imm as i64 as u64);
                    asm.orr_rrr(Reg::R13, Reg::R13, Reg::R10);
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
                    let rhs_reg = if let Some(pr) = reg_for_slot(*rhs) {
                        pr
                    } else {
                        load_int32_guarded(
                            &mut asm,
                            Reg::R10,
                            slot_offset(program, *rhs)?,
                            byte_pc,
                            acc_state,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                        Reg::R10
                    };
                    asm.cmp_rr(Reg::R13, rhs_reg);
                }
            }
            TemplateInstruction::JumpIfAccFalse { target_pc } => {
                let fused_cond = match i.checked_sub(1).and_then(|p| program.instructions.get(p)) {
                    Some(TemplateInstruction::CompareAcc { kind, .. }) => Some(match kind {
                        CompareKind::Lt => Cond::Ge,
                        CompareKind::Gt => Cond::Le,
                        CompareKind::Lte => Cond::Gt,
                        CompareKind::Gte => Cond::Lt,
                        CompareKind::EqStrict => Cond::Ne,
                    }),
                    _ => None,
                };
                if let Some(cond) = fused_cond {
                    let src = asm.b_cond_placeholder(cond);
                    branch_patches.push(BranchPatch {
                        source_offset: src,
                        target_byte_pc: *target_pc,
                        cond: Some(cond),
                    });
                } else if acc_state == AccState::Int32 {
                    asm.test_rr(Reg::R13, Reg::R13);
                    let src = asm.b_cond_placeholder(Cond::Eq);
                    branch_patches.push(BranchPatch {
                        source_offset: src,
                        target_byte_pc: *target_pc,
                        cond: Some(Cond::Eq),
                    });
                } else {
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
                match acc_state {
                    AccState::Int32 => asm.box_int32(Reg::Rax, Reg::R13),
                    AccState::Raw => asm.mov_rr(Reg::Rax, Reg::R13),
                }
                // Pop pinned regs in reverse push order. Return
                // doesn't spill pinned values back to memory (the
                // activation is destroyed on return).
                for (_, reg) in pinned.iter().rev() {
                    asm.pop(*reg);
                }
                asm.pop_callee_saved();
                asm.ret();
            }
            TemplateInstruction::LdaTagConst { value } => {
                asm.mov_imm64(Reg::R13, *value);
                acc_state = AccState::Raw;
            }
            TemplateInstruction::Mov { dst, src } => {
                let src_pr = reg_for_slot(*src);
                let dst_pr = reg_for_slot(*dst);
                match (src_pr, dst_pr) {
                    (Some(sp), Some(dp)) => {
                        asm.mov_rr(dp, sp);
                    }
                    (Some(sp), None) => {
                        asm.box_int32(Reg::R10, sp);
                        asm.mov_rm_u32(Reg::R12, slot_offset(program, *dst)?, Reg::R10);
                    }
                    (None, Some(dp)) => {
                        load_int32_guarded(
                            &mut asm,
                            dp,
                            slot_offset(program, *src)?,
                            byte_pc,
                            AccState::Raw,
                            &mut bailout_patches,
                            program.trust_int32[i],
                        );
                    }
                    (None, None) => {
                        asm.mov_mr_u32(Reg::R10, Reg::R12, slot_offset(program, *src)?);
                        asm.mov_rm_u32(Reg::R12, slot_offset(program, *dst)?, Reg::R10);
                    }
                }
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
                    asm.mov_imm64(Reg::R10, 1);
                    asm.add_rrr(Reg::R13, Reg::R13, Reg::R10);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    asm.mov_imm64(Reg::R10, 1);
                    asm.sub_rrr(Reg::R13, Reg::R13, Reg::R10);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    asm.mov_imm64(Reg::R10, 0);
                    asm.sub_rrr(Reg::R13, Reg::R10, Reg::R13);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    asm.not_r(Reg::R13);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    asm.mov_imm64(Reg::R10, *imm as i64 as u64);
                    asm.mul_rrr(Reg::R13, Reg::R13, Reg::R10);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    asm.mov_imm64(Reg::R10, *imm as i64 as u64);
                    asm.and_rrr(Reg::R13, Reg::R13, Reg::R10);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    let shift = ((*imm as u32) & 0x1F) as u8;
                    asm.shl_r32_i(Reg::R13, shift);
                    asm.sxtw(Reg::R13, Reg::R13);
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
                    let shift = ((*imm as u32) & 0x1F) as u8;
                    asm.sar_r32_i(Reg::R13, shift);
                    asm.sxtw(Reg::R13, Reg::R13);
                }
            }
            TemplateInstruction::LdaThis => {
                asm.mov_mr_u32(Reg::R13, Reg::Rbx, crate::context::offsets::THIS_RAW as u32);
                acc_state = AccState::Raw;
            }
            TemplateInstruction::LdaCurrentClosure => {
                asm.mov_mr_u32(
                    Reg::R13,
                    Reg::Rbx,
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
            }
            TemplateInstruction::CallDirect {
                callee: _,
                arg_base: _,
                arg_count: _,
            } => {
                emit_unconditional_bailout(
                    &mut asm,
                    byte_pc,
                    crate::BailoutReason::Unsupported as u32,
                    acc_state,
                    &mut bailout_patches,
                );
            }
        }
        acc_states.push(acc_state);
        i += 1;
    }

    let bailout_common = asm.position();
    // Spill each pinned reg back as a NaN-boxed int32 so the
    // interpreter resume sees a coherent frame. Mirrors the aarch64
    // bailout_common path: pinned regs hold sign-extended int32 by
    // emitter invariant, so `box_int32` produces the correctly-tagged
    // memory value. `Rax` is a convenient scratch at this point —
    // we're about to overwrite it with `BAILOUT_SENTINEL` anyway.
    for (slot, reg) in &pinned {
        asm.box_int32(Reg::Rax, *reg);
        asm.mov_rm_u32(Reg::R12, slot_offset(program, *slot)?, Reg::Rax);
    }
    asm.mov_rm_u32_32(
        Reg::Rbx,
        crate::context::offsets::BAILOUT_PC as u32,
        Reg::R10,
    );
    asm.mov_rm_u32_32(
        Reg::Rbx,
        crate::context::offsets::BAILOUT_REASON as u32,
        Reg::R11,
    );
    asm.mov_imm64(Reg::Rax, crate::BAILOUT_SENTINEL);
    for (_, reg) in pinned.iter().rev() {
        asm.pop(*reg);
    }
    asm.pop_callee_saved();
    asm.ret();

    struct PadInfo {
        entry_offset: u32,
        tail_branch_offset: u32,
    }
    let mut pad_infos: Vec<PadInfo> = Vec::with_capacity(bailout_patches.len());
    for patch in &bailout_patches {
        let pad_pos = asm.position();
        match patch.acc_state {
            AccState::Int32 => {
                asm.box_int32(Reg::R10, Reg::R13);
                asm.mov_rm_u32(
                    Reg::Rbx,
                    crate::context::offsets::ACCUMULATOR_RAW as u32,
                    Reg::R10,
                );
            }
            AccState::Raw => {
                asm.mov_rm_u32(
                    Reg::Rbx,
                    crate::context::offsets::ACCUMULATOR_RAW as u32,
                    Reg::R13,
                );
            }
        }
        asm.mov_imm64(Reg::R10, u64::from(patch.byte_pc));
        asm.mov_imm64(Reg::R11, u64::from(patch.reason));
        let tail = asm.b_placeholder();
        pad_infos.push(PadInfo {
            entry_offset: pad_pos,
            tail_branch_offset: tail,
        });
    }

    // ----- Per-loop-header OSR trampolines -----
    //
    // Mirror the main prologue (push callee-saved + pin rbx/r12/r13/r14
    // to the JitContext + registers_base + accumulator + TAG_INT32),
    // reload the spilled accumulator from `JitContext::accumulator_raw`
    // into r13, and unconditional-jump into the body offset for this
    // loop header. The body's existing `pop_callee_saved + ret` epilogue
    // tears down the same frame this trampoline set up.
    let osr_candidates = collect_osr_candidates(program, &byte_pc_to_emit);
    let mut osr_entries: Vec<OsrEntry> = Vec::with_capacity(osr_candidates.len());
    let mut osr_tail_patches: Vec<(u32 /* src */, u32 /* target */)> =
        Vec::with_capacity(osr_candidates.len());
    for (header_pc, body_off) in osr_candidates {
        let osr_entry_offset = asm.position();
        asm.push_callee_saved();
        // Save the pinning regs so the shared epilogue/bailout paths
        // can pop them uniformly.
        for (_, reg) in &pinned {
            asm.push(*reg);
        }
        asm.mov_rr(Reg::Rbx, Reg::Rdi);
        asm.mov_mr_u32(
            Reg::R12,
            Reg::Rbx,
            crate::context::offsets::REGISTERS_BASE as u32,
        );
        asm.mov_imm64(Reg::R14, TAG_INT32);
        asm.mov_mr_u32(
            Reg::R13,
            Reg::Rbx,
            crate::context::offsets::ACCUMULATOR_RAW as u32,
        );
        // Rehydrate the pinned regs from memory. The interpreter's
        // activation slots reflect the latest values because the
        // back-edge we're entering from just executed in the
        // interpreter, which syncs writes to memory.
        for (slot, reg) in &pinned {
            asm.mov_mr_u32(*reg, Reg::R12, slot_offset(program, *slot)?);
            asm.sxtw(*reg, *reg);
        }
        let src = asm.b_placeholder();
        osr_tail_patches.push((src, body_off));
        osr_entries.push(OsrEntry {
            byte_pc: header_pc,
            native_offset: osr_entry_offset,
        });
    }

    let _ = asm;

    for patch in &branch_patches {
        let Some(&(_, target_off)) = byte_pc_to_emit
            .iter()
            .find(|(bpc, _)| *bpc == patch.target_byte_pc)
        else {
            return Err(TemplateEmitError::UnresolvedBranchTarget {
                target_byte_pc: patch.target_byte_pc,
            });
        };

        let source_len = if patch.cond.is_some() { 6 } else { 5 };
        let rel_bytes = target_off as i64 - (patch.source_offset as i64 + source_len);
        if rel_bytes < i64::from(i32::MIN) || rel_bytes > i64::from(i32::MAX) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: patch.source_offset,
                target_byte_pc: patch.target_byte_pc,
            });
        }
        let rel = rel_bytes as i32 as u32;
        let patch_off = if patch.cond.is_some() {
            patch.source_offset + 2
        } else {
            patch.source_offset + 1
        };
        if !buf.patch_u32_le(patch_off, rel) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: patch.source_offset,
                target_byte_pc: patch.target_byte_pc,
            });
        }
    }

    for (patch, pad) in bailout_patches.iter().zip(pad_infos.iter()) {
        let Some(first_byte) = buf.bytes().get(patch.source_offset as usize).copied() else {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: patch.source_offset,
                target_byte_pc: pad.entry_offset,
            });
        };
        let is_conditional = first_byte == 0x0F;
        let source_len = if is_conditional { 6 } else { 5 };
        let rel_bytes = pad.entry_offset as i64 - (patch.source_offset as i64 + source_len);
        if rel_bytes < i64::from(i32::MIN) || rel_bytes > i64::from(i32::MAX) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: patch.source_offset,
                target_byte_pc: pad.entry_offset,
            });
        }
        let rel = rel_bytes as i32 as u32;
        let patch_off = if is_conditional {
            patch.source_offset + 2
        } else {
            patch.source_offset + 1
        };
        if !buf.patch_u32_le(patch_off, rel) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: patch.source_offset,
                target_byte_pc: pad.entry_offset,
            });
        }
    }

    for pad in &pad_infos {
        let rel_bytes = bailout_common as i64 - (pad.tail_branch_offset as i64 + 5);
        if rel_bytes < i64::from(i32::MIN) || rel_bytes > i64::from(i32::MAX) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: pad.tail_branch_offset,
                target_byte_pc: bailout_common,
            });
        }
        if !buf.patch_u32_le(pad.tail_branch_offset + 1, rel_bytes as i32 as u32) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: pad.tail_branch_offset,
                target_byte_pc: bailout_common,
            });
        }
    }

    // Patch each OSR trampoline's trailing `jmp body_off`.
    for &(src, target) in &osr_tail_patches {
        let rel_bytes = i64::from(target) - (i64::from(src) + 5);
        if !(i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&rel_bytes) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: src,
                target_byte_pc: target,
            });
        }
        if !buf.patch_u32_le(src + 1, rel_bytes as i32 as u32) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_byte_pc: src,
                target_byte_pc: target,
            });
        }
    }

    let _ = acc_states;

    Ok(EmittedStencil {
        code: buf,
        osr_entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_cache;
    use crate::code_memory::CompiledCodeOrigin;
    use crate::tier_up_hook::DefaultTierUpHook;
    use otter_vm::bytecode::BytecodeBuilder;
    use otter_vm::frame::FrameLayout;
    use otter_vm::interpreter::{TierUpExecResult, TierUpHook};
    use otter_vm::module::{Function, FunctionIndex};
    use otter_vm::source_compiler::ModuleCompiler;
    use otter_vm::value::RegisterValue;
    use otter_vm::{Interpreter, RuntimeState};
    use oxc_span::SourceType;

    fn compile_source_module(source: &str, url: &str) -> otter_vm::module::Module {
        ModuleCompiler::new()
            .compile(source, url, SourceType::default())
            .expect("source must compile")
    }

    fn register_window(function: &Function, args: &[i32]) -> Vec<RegisterValue> {
        let mut registers =
            vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
        let hidden = usize::from(function.frame_layout().hidden_count());
        for (i, arg) in args.iter().enumerate() {
            registers[hidden + i] = RegisterValue::from_i32(*arg);
        }
        registers
    }

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

        // M_JIT_C.3 pinning candidates: the profile-free analyzer
        // path leaves `trust_int32` all-false, which disqualifies
        // every READ reference from pinning. Cold-compile stencils
        // therefore emit the same non-pinned shape as pre-M_JIT_C.3.
        assert!(
            program.pinning_candidates.is_empty(),
            "cold analyzer must disable pinning (no feedback → trust_int32 all-false); \
             got {:?}",
            program.pinning_candidates,
        );
    }

    /// Feedback-warm analyzer path primes every arithmetic slot at
    /// `Int32`, promoting the sum loop's `s` / `i` / `n` to pinning
    /// candidates ranked by reference count inside the loop body.
    #[test]
    fn analyzer_ranks_sum_loop_pinning_candidates_with_feedback() {
        use otter_vm::feedback::{ArithmeticFeedback, FeedbackVector};

        let mut b = BytecodeBuilder::new();
        b.emit(Opcode::LdaSmi, &[Operand::Imm(0)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::LdaSmi, &[Operand::Imm(0)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();

        let loop_header = b.new_label();
        let exit = b.new_label();
        b.bind_label(loop_header).unwrap();
        let ldar_i_pc = b.emit(Opcode::Ldar, &[Operand::Reg(2)]).unwrap();
        b.attach_feedback(ldar_i_pc, otter_vm::bytecode::FeedbackSlot(0));
        let test_pc = b.emit(Opcode::TestLessThan, &[Operand::Reg(0)]).unwrap();
        b.attach_feedback(test_pc, otter_vm::bytecode::FeedbackSlot(1));
        b.emit_jump_to(Opcode::JumpIfToBooleanFalse, exit).unwrap();
        let ldar_s_pc = b.emit(Opcode::Ldar, &[Operand::Reg(1)]).unwrap();
        b.attach_feedback(ldar_s_pc, otter_vm::bytecode::FeedbackSlot(2));
        let add_pc = b.emit(Opcode::Add, &[Operand::Reg(2)]).unwrap();
        b.attach_feedback(add_pc, otter_vm::bytecode::FeedbackSlot(3));
        let orb_pc = b.emit(Opcode::BitwiseOrSmi, &[Operand::Imm(0)]).unwrap();
        b.attach_feedback(orb_pc, otter_vm::bytecode::FeedbackSlot(4));
        b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        let ldar_i2_pc = b.emit(Opcode::Ldar, &[Operand::Reg(2)]).unwrap();
        b.attach_feedback(ldar_i2_pc, otter_vm::bytecode::FeedbackSlot(5));
        let addsmi_pc = b.emit(Opcode::AddSmi, &[Operand::Imm(1)]).unwrap();
        b.attach_feedback(addsmi_pc, otter_vm::bytecode::FeedbackSlot(6));
        b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();
        b.emit_jump_to(Opcode::Jump, loop_header).unwrap();
        b.bind_label(exit).unwrap();
        let ldar_ret_pc = b.emit(Opcode::Ldar, &[Operand::Reg(1)]).unwrap();
        b.attach_feedback(ldar_ret_pc, otter_vm::bytecode::FeedbackSlot(7));
        b.emit(Opcode::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();
        let layout = FrameLayout::new(0, 1, 2, 0).unwrap();
        let feedback_layout = otter_vm::feedback::FeedbackTableLayout::new(
            (0..8)
                .map(|i| {
                    otter_vm::feedback::FeedbackSlotLayout::new(
                        otter_vm::feedback::FeedbackSlotId(i as u16),
                        otter_vm::feedback::FeedbackKind::Arithmetic,
                    )
                })
                .collect(),
        );
        let tables = otter_vm::module::FunctionTables::new(
            Default::default(),
            feedback_layout,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        let function = Function::new(Some("sum"), layout, v2, tables);

        // Prime every feedback slot at Int32 — the post-warmup shape.
        let mut fv = FeedbackVector::from_layout(function.feedback());
        for i in 0..8 {
            fv.record_arithmetic(
                otter_vm::feedback::FeedbackSlotId(i),
                ArithmeticFeedback::Int32,
            );
        }
        let program =
            analyze_template_candidate_with_feedback(&function, Some(&fv)).expect("warm analyze");

        // Inside-loop read counts (filtered by trust-int32):
        //   r2: Ldar + Add + Ldar = 3 reads.
        //   r1: Ldar = 1 read (plus a Star, which also counts for
        //        the heuristic as a write reference).
        //   r0: TestLessThan = 1 read.
        // Writes: r1 and r2 each have one Star inside the loop; r0
        // has none. Total ref counts: r2 = 3 reads + 1 write = 4;
        // r1 = 1 read + 1 write = 2; r0 = 1 read = 1.
        assert_eq!(
            program.pinning_candidates,
            vec![2, 1, 0],
            "expected warm sum-loop pinning ranking r2 > r1 > r0 (got {:?})",
            program.pinning_candidates,
        );
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

    #[test]
    fn analyzer_accepts_unary_int32_ops() {
        let mut b = BytecodeBuilder::new();
        b.emit(Opcode::LdaSmi, &[Operand::Imm(7)]).unwrap();
        b.emit(Opcode::Inc, &[]).unwrap();
        b.emit(Opcode::Dec, &[]).unwrap();
        b.emit(Opcode::Negate, &[]).unwrap();
        b.emit(Opcode::BitwiseNot, &[]).unwrap();
        b.emit(Opcode::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();
        let layout = FrameLayout::new(0, 0, 0, 0).unwrap();
        let function = Function::with_empty_tables(Some("unary"), layout, v2);

        let program = analyze_template_candidate(&function).expect("analyze unary ops");
        assert_eq!(
            program.instructions,
            vec![
                TemplateInstruction::LdaI32 { imm: 7 },
                TemplateInstruction::IncAcc,
                TemplateInstruction::DecAcc,
                TemplateInstruction::NegateAcc,
                TemplateInstruction::BitNotAcc,
                TemplateInstruction::ReturnAcc,
            ]
        );
    }

    #[test]
    fn analyzer_accepts_smi_immediate_ops() {
        let mut b = BytecodeBuilder::new();
        b.emit(Opcode::LdaSmi, &[Operand::Imm(3)]).unwrap();
        b.emit(Opcode::MulSmi, &[Operand::Imm(4)]).unwrap();
        b.emit(Opcode::BitwiseAndSmi, &[Operand::Imm(15)]).unwrap();
        b.emit(Opcode::ShlSmi, &[Operand::Imm(2)]).unwrap();
        b.emit(Opcode::ShrSmi, &[Operand::Imm(1)]).unwrap();
        b.emit(Opcode::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();
        let layout = FrameLayout::new(0, 0, 0, 0).unwrap();
        let function = Function::with_empty_tables(Some("smi_ops"), layout, v2);

        let program = analyze_template_candidate(&function).expect("analyze smi ops");
        assert_eq!(
            program.instructions,
            vec![
                TemplateInstruction::LdaI32 { imm: 3 },
                TemplateInstruction::MulAccI32 { imm: 4 },
                TemplateInstruction::BitAndAccI32 { imm: 15 },
                TemplateInstruction::ShlAccI32 { imm: 2 },
                TemplateInstruction::ShrAccI32 { imm: 1 },
                TemplateInstruction::ReturnAcc,
            ]
        );
    }

    #[test]
    fn analyzer_accepts_call_direct_as_deopt_boundary() {
        let mut b = BytecodeBuilder::new();
        b.emit(
            Opcode::CallDirect,
            &[Operand::Idx(1), Operand::RegList { base: 2, count: 1 }],
        )
        .unwrap();
        b.emit(Opcode::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();
        let layout = FrameLayout::new(0, 0, 0, 0).unwrap();
        let function = Function::with_empty_tables(Some("caller"), layout, v2);

        let program = analyze_template_candidate(&function).expect("analyze call-direct");
        assert_eq!(
            program.instructions,
            vec![
                TemplateInstruction::CallDirect {
                    callee: FunctionIndex(1),
                    arg_base: 2,
                    arg_count: 1,
                },
                TemplateInstruction::ReturnAcc,
            ]
        );
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
        let stencil = emit_template_stencil(&program).expect("emit");
        // The sum-loop has exactly one loop header, and that header
        // begins with `Ldar r2` — an OSR-safe op — so the emitter must
        // produce one OSR entry trampoline.
        assert_eq!(
            stencil.osr_entries.len(),
            1,
            "expected one OSR entry for the single loop header",
        );
        let buf = stencil.code;
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
        // of pads, so ≈280 + 8·12 + 8·32 ≈ 632 B for the body alone.
        // M_JIT_C.1 adds one OSR trampoline (~10 insns / 40 bytes) per
        // OSR-eligible loop header, so lock the upper bound at 720 to
        // catch regressions while leaving headroom for the trampolines.
        assert!(
            bytes.len() <= 720,
            "v2 sum-loop stencil larger than expected: {} bytes (M_JIT_C.1 target ≤ 720)",
            bytes.len()
        );
    }

    /// End-to-end smoke test that routes a source-compiled JS function
    /// through the production `otter` CLI, which in turn executes the
    /// hot inner function via `DefaultTierUpHook::execute_cached`.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn stencil_invocation_smoke() {
        use std::fs;
        use std::process::Command;

        let script_path = std::env::temp_dir().join("otter-stencil-invocation-smoke.js");
        let script = "function sum(n) { \
                          let s = 0; \
                          let i = 0; \
                          while (i < n) { \
                              s = (s + i) | 0; \
                              i = i + 1; \
                          } \
                          return s; \
                      } \
                      function main() { \
                          let i = 0; \
                          let out = 0; \
                          while (i < 120) { \
                              out = sum(1000); \
                              i = i + 1; \
                          } \
                          return out; \
                      }";
        fs::write(&script_path, script).expect("write smoke script");

        let output = Command::new("cargo")
            .args([
                "run",
                "-p",
                "otterjs",
                "--",
                "--dump-jit-stats",
                "run",
                script_path.to_str().expect("utf8 temp path"),
            ])
            .output()
            .expect("run otter smoke script");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "otter smoke run failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            stderr,
        );
        assert!(
            stderr.contains("sum"),
            "expected telemetry to mention compiled sum():\n{stderr}",
        );
        assert!(
            stderr.contains("Native ratio"),
            "expected telemetry execution summary:\n{stderr}",
        );

        let _ = fs::remove_file(&script_path);
    }

    /// End-to-end smoke test for M_JIT_C.1 mid-loop OSR.
    ///
    /// The script has a single top-level function (`main`) called once,
    /// containing one int32 accumulator loop with no inner calls. Because
    /// the function is the entry point, `run_with_tier_up` never fires
    /// (it only intercepts inner `CallDirect`); the only path into the
    /// JIT is the back-edge OSR added by M_JIT_C.1. We pick a loop count
    /// well above the JSC-style `TIER1_INITIAL_BUDGET = 1500` back-edge
    /// budget so the OSR trampoline is guaranteed to fire mid-loop.
    ///
    /// We assert via `--dump-jit-stats` that the telemetry shows a
    /// non-zero JIT entry count for `main` — proving the back-edge entry
    /// path was taken — and that the script returns the correct sum.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn osr_smoke() {
        use std::fs;
        use std::process::Command;

        let script_path = std::env::temp_dir().join("otter-osr-smoke.js");
        // 100_000 iterations is well above the 1500 back-edge budget;
        // OSR must have fired by the time the loop exits.
        // (`(s + i) | 0` keeps the accumulator int32-tagged so the JIT
        // path stays on the trust-int32 fast path.)
        let script = "function main() { \
                          let s = 0; \
                          let i = 0; \
                          while (i < 100000) { \
                              s = (s + i) | 0; \
                              i = i + 1; \
                          } \
                          return s; \
                      }";
        fs::write(&script_path, script).expect("write smoke script");

        let output = Command::new("cargo")
            .args([
                "run",
                "-p",
                "otterjs",
                "--",
                "--dump-jit-stats",
                "run",
                script_path.to_str().expect("utf8 temp path"),
            ])
            .output()
            .expect("run otter osr smoke script");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "otter osr smoke run failed:\nstdout:\n{stdout}\nstderr:\n{stderr}",
        );
        // Telemetry summary must mention `main` (the only function the
        // hook can have entered) so we know JIT execution actually ran.
        assert!(
            stderr.contains("main"),
            "expected telemetry to mention compiled main():\n{stderr}",
        );
        assert!(
            stderr.contains("Native ratio"),
            "expected telemetry execution summary:\n{stderr}",
        );
        // Tighter check: the Native-ratio line must report at least one
        // JIT entry. The line shape is `Native ratio:  X%  (J JIT / I
        // interpreter entries)`. Reject `0 JIT` to confirm the OSR path
        // actually fired — a regression that disables back-edge OSR
        // would still produce a Native-ratio line, but with `0 JIT`.
        assert!(
            !stderr.contains("0 JIT"),
            "expected at least one JIT entry from OSR — Native ratio shows zero JIT entries:\n{stderr}",
        );

        let _ = fs::remove_file(&script_path);
    }

    /// M_JIT_C.2 regression guard: a feedback-warm recompile of the
    /// `bench2.ts sum` loop produces a strictly smaller stencil than
    /// the cold-compile variant, because every arithmetic op in the
    /// hot loop has its tag guard elided once `ArithmeticFeedback::Int32`
    /// stabilises.
    ///
    /// The test synthesises a fully-primed `FeedbackVector` rather than
    /// running the interpreter (so it stays a unit test, not an
    /// integration test). The feedback layout comes from the source
    /// compiler's slot allocation; we populate every arithmetic slot
    /// with `Int32`. Shrink target: the warm stencil must be at most
    /// 80% of the cold stencil (matching the 20% shrink goal in the
    /// M_JIT_C.2 acceptance criteria).
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn m_jit_c_2_feedback_shrinks_stencil() {
        use otter_vm::feedback::{ArithmeticFeedback, FeedbackSlotData, FeedbackVector};

        let module = compile_source_module(
            "function sum(n) { \
                 let s = 0; \
                 let i = 0; \
                 while (i < n) { \
                     s = (s + i) | 0; \
                     i = i + 1; \
                 } \
                 return s; \
             }",
            "bench2.js",
        );
        let sum = module.function(FunctionIndex(0)).expect("sum function");

        // Cold compile: no feedback → every arithmetic op keeps its
        // tag guard.
        let cold_program = analyze_template_candidate(sum).expect("cold analyze");
        assert!(
            cold_program.trust_int32.iter().all(|&t| !t),
            "cold analyzer must leave trust_int32 all-false",
        );
        let cold_stencil = emit_template_stencil(&cold_program).expect("cold emit");
        let cold_size = cold_stencil.code.bytes().len();

        // Warm recompile: synthesise a fully-primed feedback vector
        // with `Int32` observations on every arithmetic slot, matching
        // what the interpreter records after running the loop through
        // enough iterations for the JSC-style back-edge budget to
        // exhaust.
        let layout = sum.feedback();
        assert!(
            !layout.is_empty(),
            "source compiler must populate arithmetic feedback slots for sum's loop body",
        );
        let mut fv = FeedbackVector::from_layout(layout);
        for (i, slot) in layout.slots().iter().enumerate() {
            if let FeedbackSlotData::Arithmetic(_) = FeedbackSlotData::for_kind(slot.kind()) {
                fv.record_arithmetic(
                    otter_vm::feedback::FeedbackSlotId(i as u16),
                    ArithmeticFeedback::Int32,
                );
            }
        }
        let warm_program =
            analyze_template_candidate_with_feedback(sum, Some(&fv)).expect("warm analyze");
        assert!(
            warm_program.trust_int32.iter().any(|&t| t),
            "warm analyzer must flip at least one trust_int32 entry to true",
        );
        let warm_stencil = emit_template_stencil(&warm_program).expect("warm emit");
        let warm_size = warm_stencil.code.bytes().len();

        // The emitter's guard elision is what drives the shrink: each
        // guarded load is `eor / tst / b.cond` on aarch64 (12 bytes of
        // guard code) plus the shared bailout pad it targets (~24
        // bytes). Even half of the sum loop's arithmetic ops going
        // trust-int32 shaves well past 20%.
        let warm_ratio = (warm_size as f64) / (cold_size as f64);
        assert!(
            warm_size < cold_size,
            "warm stencil must be smaller than cold: cold={cold_size} warm={warm_size}",
        );
        assert!(
            warm_ratio <= 0.80,
            "feedback-warm recompile should shrink stencil by ≥ 20%: \
             cold={cold_size} warm={warm_size} ratio={warm_ratio:.2}",
        );
        eprintln!(
            "M_JIT_C.2 shrink: cold={cold_size} B → warm={warm_size} B \
             ({:.1}% reduction)",
            (1.0 - warm_ratio) * 100.0,
        );
    }

    /// M_JIT_C.3 disassembly sanity: the feedback-warm `bench2 sum`
    /// stencil must pin the hot `s` and `i` slots into callee-saved
    /// registers, which means the inner loop body has zero `ldr`
    /// instructions that reference the registers_base pointer (`x9`)
    /// for those pinned slots.
    ///
    /// We use a proxy metric — total `ldr` count inside the body —
    /// rather than an exact slot-by-slot match: counting bare
    /// mnemonics stays robust to minor emitter reordering, and an
    /// unpinned sum stencil is unambiguous on this metric (it has
    /// ~8 slot loads; a pinned stencil has ≤ 2, counting only the
    /// `n` parameter's compare load and the return-path `s` load).
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn m_jit_c_3_pinned_body_skips_pinned_slot_loads() {
        use otter_vm::feedback::{ArithmeticFeedback, FeedbackSlotData, FeedbackVector};

        let module = compile_source_module(
            "function sum(n) { \
                 let s = 0; \
                 let i = 0; \
                 while (i < n) { \
                     s = (s + i) | 0; \
                     i = i + 1; \
                 } \
                 return s; \
             }",
            "bench2.js",
        );
        let sum = module.function(FunctionIndex(0)).expect("sum function");

        // Warm feedback so the analyzer promotes pinning.
        let layout = sum.feedback();
        let mut fv = FeedbackVector::from_layout(layout);
        for (i, slot) in layout.slots().iter().enumerate() {
            if matches!(
                FeedbackSlotData::for_kind(slot.kind()),
                FeedbackSlotData::Arithmetic(_)
            ) {
                fv.record_arithmetic(
                    otter_vm::feedback::FeedbackSlotId(i as u16),
                    ArithmeticFeedback::Int32,
                );
            }
        }
        let program =
            analyze_template_candidate_with_feedback(sum, Some(&fv)).expect("warm analyze");
        assert!(
            !program.pinning_candidates.is_empty(),
            "warm sum should produce pinning candidates (got {:?})",
            program.pinning_candidates,
        );
        let stencil = emit_template_stencil(&program).expect("warm emit");
        let bytes = stencil.code.bytes();

        // Count `LDR`s in the whole stencil. A feedback-warm sum
        // pins `r2` (`i`) and `r1` (`s`); the remaining memory
        // reads are:
        //   * 1 load of `registers_base` from the JitContext
        //     prologue.
        //   * 1 load of `n` (r0) for TestLessThan inside the loop
        //     (r0 has fewer references than s/i so it's the 3rd
        //     pinning candidate — but the emitter only pins 2 on
        //     this arch).
        //   * 2 loads during the prologue to populate the pinned
        //     regs' initial values (`s` and `i`, both int32).
        //   * 1 load to rehydrate `accumulator_raw` on each OSR
        //     trampoline.
        // An unpinned stencil has ~8+ `LDR`s inside the loop alone
        // (Ldar×2, Add, TestLessThan, Star×2, etc.), plus prologue
        // and epilogue loads. Measured warm-pinned total: ~11 LDRs
        // (prologue reg-base + 2 pin loads + n load in TestLessThan
        // + ldr_x20 in Return + ldr_x20 in bailout_common + 4 in the
        // OSR trampoline). Lock the threshold at ≤ 12 so the test
        // fails if the loop body re-introduces per-iteration slot
        // loads.
        let ldr_count = bytes
            .chunks_exact(4)
            .filter_map(|chunk| {
                let word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                bad64::decode(word, 0).ok()
            })
            .filter(|insn| format!("{:?}", insn.op()) == "LDR")
            .count();
        assert!(
            ldr_count <= 12,
            "feedback-warm pinned sum should have ≤ 12 LDR insns (got {ldr_count}); \
             an unpinned stencil has ≥ 15 (the loop alone has ~8)",
        );
        eprintln!("M_JIT_C.3 pinned sum stencil: {} LDR insns", ldr_count);
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn recursion_factorial_caches_template_baseline_entry() {
        code_cache::clear();

        let module = compile_source_module(
            "function fact(n) { \
                 if (n === 0) { return 1; } \
                 return n * fact(n - 1); \
             } \
             function main() { return fact(7); }",
            "fact.js",
        );
        let fact = module.function(FunctionIndex(0)).expect("fact function");
        let fact_ptr = fact as *const Function;

        let mut runtime = RuntimeState::new();
        let hook = DefaultTierUpHook;
        assert!(hook.try_compile(
            &module,
            FunctionIndex(0),
            (&mut runtime as *mut RuntimeState).cast::<()>(),
        ));
        assert_eq!(
            code_cache::origin_of(fact_ptr),
            Some(CompiledCodeOrigin::TemplateBaseline),
        );

        code_cache::clear();
    }

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
        let module = compile_source_module("function f(n) { return n + 1; }", "f.js");
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

        let stencil = emit_template_stencil(&program).expect("emit stencil");
        // The M1 source has no loops, so the emitter must not synthesize
        // any OSR trampolines.
        assert!(
            stencil.osr_entries.is_empty(),
            "M1 stencil must have no OSR entries (no loops)",
        );
        let buf = stencil.code;
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

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn m2_stencil_disassembly_sanity() {
        use iced_x86::{Decoder, DecoderOptions, Mnemonic};

        let module = compile_source_module("function f(n) { return n + 1; }", "f.js");
        let function = module
            .function(FunctionIndex(0))
            .expect("module has entry function");

        let program = analyze_template_candidate(function).expect("analyze");
        assert_eq!(
            program.instructions.as_slice(),
            &[
                TemplateInstruction::Ldar { reg: 0 },
                TemplateInstruction::AddAccI32 { imm: 1 },
                TemplateInstruction::ReturnAcc,
            ],
            "analyzer must lower the M1 source to a Ldar / AddSmi / Return triple",
        );

        let stencil = emit_template_stencil(&program).expect("emit stencil");
        assert!(
            stencil.osr_entries.is_empty(),
            "M1 stencil must have no OSR entries (no loops)",
        );
        let buf = stencil.code;
        let bytes = buf.bytes();
        assert!(!bytes.is_empty(), "emitter produced no code");

        let mut decoder = Decoder::with_ip(64, bytes, 0, DecoderOptions::NONE);
        let mut mnemonics = Vec::new();
        while decoder.can_decode() {
            let insn = decoder.decode();
            mnemonics.push(insn.mnemonic());
        }

        let has = |needle: Mnemonic| mnemonics.contains(&needle);

        assert!(
            has(Mnemonic::Mov),
            "prologue/immediates missing MOV: {mnemonics:?}"
        );
        assert!(has(Mnemonic::Xor), "guard missing XOR: {mnemonics:?}");
        assert!(has(Mnemonic::Shr), "guard missing SHR: {mnemonics:?}");
        assert!(has(Mnemonic::Jne), "guard missing JNE: {mnemonics:?}");
        assert!(
            has(Mnemonic::Movsxd),
            "missing MOVSXD sign-extension: {mnemonics:?}",
        );
        assert!(has(Mnemonic::Add), "missing ADD: {mnemonics:?}");
        assert!(has(Mnemonic::Or), "missing OR (box_int32): {mnemonics:?}");
        assert!(has(Mnemonic::Ret), "missing RET: {mnemonics:?}");

        assert!(
            bytes.len() <= 220,
            "M1 x86_64 stencil larger than expected: {} bytes (target ≤ 220)",
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
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[ignore = "M1 microbenchmark — run manually via `--ignored m1_microbench --nocapture`"]
    #[test]
    fn m1_microbench() {
        use std::time::Instant;

        let module = compile_source_module("function f(n) { return n + 1; }", "f.js");
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
    /// int32 accumulator loop reserved for M7's full benchmark vs
    /// `bun` and `node` (see `V2_MIGRATION.md`). Measures the
    /// per-call latency of `sum(1_000_000)` first through the v2
    /// interpreter, then through a cached `DefaultTierUpHook`
    /// `execute_cached` entry, printing both `bench2 interp:` and
    /// `bench2 jit:` rows for the V2_MIGRATION.md tracker.
    ///
    /// Invoke with:
    /// ```text
    /// cargo test -p otter-jit --release -- --ignored bench2_microbench --nocapture
    /// ```
    ///
    /// The JIT half compiles the source-compiled `sum()` helper via
    /// `DefaultTierUpHook::try_compile`, then times
    /// `DefaultTierUpHook::execute_cached` directly so the benchmark
    /// measures the production entry path without reintroducing raw
    /// stencil calls from the harness.
    ///
    /// Test-only env overrides:
    /// `OTTER_BENCH2_N`, `OTTER_BENCH2_WARMUP_CALLS`, `OTTER_BENCH2_CALLS`.
    /// Defaults stay pinned to the tracker rows; the overrides exist so
    /// slower cross-target runs (for example Rosetta x86_64 on Apple
    /// Silicon) can still produce a local comparison inside the fixed
    /// timeout budget.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[ignore = "M7 bench2 microbench — run manually via `--ignored bench2_microbench --nocapture`"]
    #[test]
    fn bench2_microbench() {
        use otter_vm::module::FunctionIndex;
        use otter_vm::value::RegisterValue;
        use otter_vm::{Interpreter, RuntimeState};
        use std::time::Instant;

        fn env_u32(name: &str, default: u32) -> u32 {
            std::env::var(name)
                .ok()
                .and_then(|raw| raw.parse::<u32>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(default)
        }

        // Canonical M7 source (see V2_MIGRATION.md), written with
        // single-declarator lets so it stays parseable on the
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
        let module = compile_source_module(source, "bench2.ts");
        let function = module
            .function(FunctionIndex(0))
            .expect("module has entry function");
        let layout = function.frame_layout();
        let hidden = usize::from(layout.hidden_count());
        let mut registers = vec![RegisterValue::undefined(); usize::from(layout.register_count())];
        // Loop limit. Match V2_MIGRATION.md's "10⁶ iter" target so
        // the latency row is comparable to the eventual bun / node
        // numbers.
        let n = i32::try_from(env_u32("OTTER_BENCH2_N", 1_000_000)).unwrap_or(1_000_000);
        registers[hidden] = RegisterValue::from_i32(n);

        let interpreter = Interpreter::new();
        let mut runtime = RuntimeState::new();

        // Warmup — `sum(N)` runs N iterations internally, so 100
        // calls = 10⁸ inner iterations. Plenty to prime any
        // thread-local state in the interpreter.
        let warmup_calls = env_u32("OTTER_BENCH2_WARMUP_CALLS", 100);
        for _ in 0..warmup_calls {
            let result = interpreter
                .execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
                .expect("warmup execute");
            let _ = result.return_value();
        }

        // Measure: 50 calls × 10⁶ inner iterations = 5×10⁷ inner
        // iters total. Per-call latency is the headline number;
        // per-inner-iter is reported alongside for direct
        // comparison with bun/node sum-loop benchmarks.
        let calls = env_u32("OTTER_BENCH2_CALLS", 50);
        let started = Instant::now();
        let mut acc: i64 = 0;
        for _ in 0..calls {
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
        let total_inner_iters = u128::from(calls) * u128::from(n as u32);
        let per_call_ns = total_ns / u128::from(calls);
        let per_inner_iter_ns = total_ns / total_inner_iters;
        println!(
            "bench2 interp: {per_call_ns} ns/call ({per_inner_iter_ns} ns/inner-iter, \
             {} ms total over {calls} calls × {n} iter, acc={acc})",
            elapsed.as_millis(),
        );

        // Keep reusing the interpreter runtime so `try_compile` sees
        // the persistent `FeedbackVector` built up by the warmup +
        // measurement loops above. That's what activates M_JIT_C.2
        // trust-int32 elision and M_JIT_C.3 loop-local pinning on
        // recompile — a fresh `RuntimeState` would compile cold and
        // miss both.
        code_cache::clear();
        let hook = DefaultTierUpHook;
        assert!(hook.try_compile(
            &module,
            FunctionIndex(0),
            (&mut runtime as *mut RuntimeState).cast::<()>(),
        ));

        let mut jit_registers = register_window(function, &[n]);
        for _ in 0..warmup_calls {
            match hook.execute_cached(
                &module,
                FunctionIndex(0),
                jit_registers.as_mut_ptr(),
                jit_registers.len(),
                RegisterValue::undefined().raw_bits(),
                (&mut runtime as *mut RuntimeState).cast::<()>(),
                std::ptr::null(),
            ) {
                TierUpExecResult::Return(_) => {}
                other => panic!("warmup JIT execute failed: {other:?}"),
            }
        }

        let jit_started = Instant::now();
        let mut jit_acc: i64 = 0;
        for _ in 0..calls {
            jit_registers[hidden] = RegisterValue::from_i32(n);
            match hook.execute_cached(
                &module,
                FunctionIndex(0),
                jit_registers.as_mut_ptr(),
                jit_registers.len(),
                RegisterValue::undefined().raw_bits(),
                (&mut runtime as *mut RuntimeState).cast::<()>(),
                std::ptr::null(),
            ) {
                TierUpExecResult::Return(value) => {
                    jit_acc = jit_acc.wrapping_add(i64::from(value.as_i32().unwrap_or(0)));
                }
                other => panic!("measured JIT execute failed: {other:?}"),
            }
        }
        let jit_elapsed = jit_started.elapsed();
        assert_ne!(jit_acc, 0, "JIT sum returned zero unexpectedly");

        let jit_total_ns = jit_elapsed.as_nanos();
        let jit_per_call_ns = jit_total_ns / u128::from(calls);
        let jit_per_inner_iter_ns = jit_total_ns / total_inner_iters;
        println!(
            "bench2 jit: {jit_per_call_ns} ns/call ({jit_per_inner_iter_ns} ns/inner-iter, \
             {} ms total over {calls} calls × {n} iter, acc={jit_acc})",
            jit_elapsed.as_millis(),
        );
        assert!(
            jit_per_inner_iter_ns < per_inner_iter_ns,
            "JIT bench2 should beat interpreter: jit={jit_per_inner_iter_ns} ns/iter, interp={per_inner_iter_ns} ns/iter",
        );

        code_cache::clear();
    }
}
