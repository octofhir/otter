//! v1 → v2 bytecode transpiler. **Bootstrap** path for Phase 2 of the
//! Ignition-style ISA migration.
//!
//! Instead of rewriting the 5.5k-LOC `source_compiler` from scratch
//! (direct AST → v2, Phase 2b), this transpiler walks a v1 `Bytecode`
//! stream and emits the equivalent v2 stream, following the deterministic
//! mapping in `docs/bytecode-v2.md` §7.
//!
//! Why a transpiler first:
//! 1. **Validates the v2 ISA.** If every v1 opcode maps cleanly, the ISA
//!    is expressively complete. Any hole here signals a v2 design fix
//!    before we pay the cost of the AST rewrite.
//! 2. **Unblocks Phase 3.** The dispatch_v2 interpreter can start
//!    consuming real v2 bytecode (from any v1-compiled script) today.
//! 3. **Living reference.** The v1→v2 mapping spec in `bytecode-v2.md`
//!    becomes executable — every production rule is a function arm here.
//!
//! Scope for Phase 2a: opcodes that appear in the `arithmetic_loop.ts`
//! benchmark's hot inner functions (our motivating workload). Other
//! opcodes return `TranspileError::Unsupported` until Phase 2a.3 extends
//! coverage.

use crate::bytecode::{
    Bytecode as V1Bytecode, Instruction as V1Instruction, Opcode as V1Opcode, ProgramCounter,
};
use crate::module::Function;

use super::encoding::{BytecodeBuilder, EncodeError, Label};
use super::opcodes::OpcodeV2;
use super::operand::Operand;
use super::Bytecode;

/// Errors the transpiler surfaces. `Unsupported` is the intentional hole
/// that narrows over Phase 2a.3.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TranspileError {
    /// The v1 opcode doesn't yet have a v2 lowering here. Extend as
    /// needed — the full mapping lives in `docs/bytecode-v2.md` §7.
    #[error("v1 opcode {opcode:?} at pc {pc} not yet supported by transpiler")]
    Unsupported { pc: u32, opcode: V1Opcode },
    /// A jump in v1 points outside the v1 bytecode stream.
    #[error("invalid jump target at pc {pc}: offset {offset}")]
    InvalidJumpTarget { pc: u32, offset: i32 },
    /// A call-family opcode requires side-table context but the caller
    /// used [`transpile`] (not [`transpile_with_function`]).
    #[error(
        "v1 opcode {opcode:?} at pc {pc} needs side-table context; use transpile_with_function"
    )]
    MissingFunctionContext { pc: u32, opcode: V1Opcode },
    /// Call-site metadata missing from `function.calls()` for a PC that
    /// carries a call-family opcode.
    #[error("call metadata missing for {opcode:?} at pc {pc}")]
    MissingCallMetadata { pc: u32, opcode: V1Opcode },
    /// Bubble up from the underlying v2 encoder.
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// Transpile one v1 bytecode stream into a v2 bytecode stream.
///
/// The transpiler is deterministic: the same v1 input always produces
/// the same v2 output. That makes it safe as a regression oracle while
/// Phase 2b develops the direct AST → v2 compiler in parallel.
///
/// Pass `None` for `function` to transpile without side-table context.
/// That shortcut handles every opcode whose data lives entirely in
/// `instr.a/b/c` / immediate — ≈110 of the 120 v1 opcodes. The ≈10
/// opcodes that read the `CallTable` (`CallDirect`, `CallClosure`,
/// `CallSpread`, `CallSuper`, `CallSuperSpread`, `CallEval`,
/// `TailCallClosure`) require `Some(function)` or surface
/// `TranspileError::MissingFunctionContext`.
pub fn transpile(v1: &V1Bytecode) -> Result<Bytecode, TranspileError> {
    transpile_impl(v1, None)
}

/// Function-aware transpile. Produces the same bytecode as
/// [`transpile`] for self-contained opcodes and additionally lowers
/// every call-family opcode by resolving its side-table entry in
/// `function.calls()`.
pub fn transpile_with_function(
    v1: &V1Bytecode,
    function: &Function,
) -> Result<Bytecode, TranspileError> {
    transpile_impl(v1, Some(function))
}

fn transpile_impl(
    v1: &V1Bytecode,
    function: Option<&Function>,
) -> Result<Bytecode, TranspileError> {
    let instructions = v1.instructions();
    let mut b = BytecodeBuilder::new();

    // Every v1 pc is a candidate jump target. Allocate a Label for each,
    // bind it as we visit the pc, and back-patch forward references via
    // `emit_jump_to`. Backward jumps to already-visited pcs resolve
    // immediately.
    let labels: Vec<Label> = (0..instructions.len()).map(|_| b.new_label()).collect();

    for (v1_pc, instr) in instructions.iter().enumerate() {
        b.bind_label(labels[v1_pc])?;
        emit_v1(
            &mut b,
            v1_pc as u32,
            *instr,
            &labels,
            instructions.len(),
            function,
        )?;
    }

    Ok(b.finish()?)
}

fn emit_v1(
    b: &mut BytecodeBuilder,
    pc: u32,
    instr: V1Instruction,
    labels: &[Label],
    total: usize,
    function: Option<&Function>,
) -> Result<(), TranspileError> {
    let a = u32::from(instr.a());
    let bx = u32::from(instr.b());
    let c = u32::from(instr.c());

    match instr.opcode() {
        // --- constant / receiver loads: LdaX; Star a ---
        V1Opcode::LoadThis => {
            b.emit(OpcodeV2::LdaThis, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadCurrentClosure => {
            b.emit(OpcodeV2::LdaCurrentClosure, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadNewTarget => {
            b.emit(OpcodeV2::LdaNewTarget, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadException => {
            b.emit(OpcodeV2::LdaException, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadUndefined => {
            b.emit(OpcodeV2::LdaUndefined, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadNull => {
            b.emit(OpcodeV2::LdaNull, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadTrue => {
            b.emit(OpcodeV2::LdaTrue, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadFalse => {
            b.emit(OpcodeV2::LdaFalse, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadHole => {
            b.emit(OpcodeV2::LdaTheHole, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadNaN => {
            b.emit(OpcodeV2::LdaNaN, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadI32 => {
            b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(instr.immediate_i32())])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadString => {
            b.emit(OpcodeV2::LdaConstStr, &[Operand::Idx(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadF64 => {
            b.emit(OpcodeV2::LdaConstF64, &[Operand::Idx(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::LoadBigInt => {
            b.emit(OpcodeV2::LdaConstBigInt, &[Operand::Idx(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }

        // --- register move ---
        V1Opcode::Move => {
            // `Move a, b` — slot a = slot b. Use Mov (no acc clobber).
            b.emit(OpcodeV2::Mov, &[Operand::Reg(bx), Operand::Reg(a)])?;
        }

        // --- binary arithmetic: Ldar b; Op c; Star a ---
        V1Opcode::Add => emit_binop(b, OpcodeV2::Add, a, bx, c)?,
        V1Opcode::Sub => emit_binop(b, OpcodeV2::Sub, a, bx, c)?,
        V1Opcode::Mul => emit_binop(b, OpcodeV2::Mul, a, bx, c)?,
        V1Opcode::Div => emit_binop(b, OpcodeV2::Div, a, bx, c)?,
        V1Opcode::Mod => emit_binop(b, OpcodeV2::Mod, a, bx, c)?,
        V1Opcode::Exp => emit_binop(b, OpcodeV2::Exp, a, bx, c)?,
        V1Opcode::BitAnd => emit_binop(b, OpcodeV2::BitwiseAnd, a, bx, c)?,
        V1Opcode::BitOr => emit_binop(b, OpcodeV2::BitwiseOr, a, bx, c)?,
        V1Opcode::BitXor => emit_binop(b, OpcodeV2::BitwiseXor, a, bx, c)?,
        V1Opcode::Shl => emit_binop(b, OpcodeV2::Shl, a, bx, c)?,
        V1Opcode::Shr => emit_binop(b, OpcodeV2::Shr, a, bx, c)?,
        V1Opcode::UShr => emit_binop(b, OpcodeV2::UShr, a, bx, c)?,

        // --- comparisons: Ldar b; TestOp c; Star a ---
        V1Opcode::Eq => emit_binop(b, OpcodeV2::TestEqualStrict, a, bx, c)?,
        V1Opcode::LooseEq => emit_binop(b, OpcodeV2::TestEqual, a, bx, c)?,
        V1Opcode::Lt => emit_binop(b, OpcodeV2::TestLessThan, a, bx, c)?,
        V1Opcode::Gt => emit_binop(b, OpcodeV2::TestGreaterThan, a, bx, c)?,
        V1Opcode::Lte => emit_binop(b, OpcodeV2::TestLessThanOrEqual, a, bx, c)?,
        V1Opcode::Gte => emit_binop(b, OpcodeV2::TestGreaterThanOrEqual, a, bx, c)?,
        V1Opcode::InstanceOf => emit_binop(b, OpcodeV2::TestInstanceOf, a, bx, c)?,
        V1Opcode::HasProperty => emit_binop(b, OpcodeV2::TestIn, a, bx, c)?,

        // --- unary ---
        V1Opcode::Not => emit_unary(b, OpcodeV2::LogicalNot, a, bx)?,
        V1Opcode::TypeOf => emit_unary(b, OpcodeV2::TypeOf, a, bx)?,
        V1Opcode::ToNumber => emit_unary(b, OpcodeV2::ToNumber, a, bx)?,
        V1Opcode::ToString => emit_unary(b, OpcodeV2::ToString, a, bx)?,
        V1Opcode::ToPropertyKey => emit_unary(b, OpcodeV2::ToPropertyKey, a, bx)?,

        // --- control flow ---
        V1Opcode::Jump => {
            let target = resolve_jump_target(pc, instr.immediate_i32(), total)?;
            b.emit_jump_to(OpcodeV2::Jump, labels[target])?;
        }
        V1Opcode::JumpIfTrue => {
            // v1 conditional reads slot A, jumps if truthy. v2 tests acc
            // via JumpIfToBooleanTrue, so load slot A into acc first.
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            let target = resolve_jump_target(pc, instr.immediate_i32(), total)?;
            b.emit_jump_to(OpcodeV2::JumpIfToBooleanTrue, labels[target])?;
        }
        V1Opcode::JumpIfFalse => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            let target = resolve_jump_target(pc, instr.immediate_i32(), total)?;
            b.emit_jump_to(OpcodeV2::JumpIfToBooleanFalse, labels[target])?;
        }
        V1Opcode::Return => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            b.emit(OpcodeV2::Return, &[])?;
        }
        V1Opcode::Throw => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            b.emit(OpcodeV2::Throw, &[])?;
        }
        V1Opcode::Nop => {
            b.emit(OpcodeV2::Nop, &[])?;
        }

        // --- globals ---
        V1Opcode::GetGlobal => {
            b.emit(OpcodeV2::LdaGlobal, &[Operand::Idx(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::SetGlobal => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            b.emit(OpcodeV2::StaGlobal, &[Operand::Idx(bx)])?;
        }
        V1Opcode::SetGlobalStrict => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            b.emit(OpcodeV2::StaGlobalStrict, &[Operand::Idx(bx)])?;
        }
        V1Opcode::TypeOfGlobal => {
            b.emit(OpcodeV2::TypeOfGlobal, &[Operand::Idx(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }

        // --- upvalues ---
        V1Opcode::GetUpvalue => {
            b.emit(OpcodeV2::LdaUpvalue, &[Operand::Idx(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::SetUpvalue => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            b.emit(OpcodeV2::StaUpvalue, &[Operand::Idx(bx)])?;
        }

        // --- property access ---
        V1Opcode::GetProperty => {
            b.emit(
                OpcodeV2::LdaNamedProperty,
                &[Operand::Reg(bx), Operand::Idx(c)],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::SetProperty => {
            // v1 `SetProperty a, b, idx` stores slot a into slot b under
            // property name idx. v2 `StaNamedProperty` takes acc as the
            // value and `(target reg, name idx)` as operands.
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            b.emit(
                OpcodeV2::StaNamedProperty,
                &[Operand::Reg(bx), Operand::Idx(c)],
            )?;
        }
        V1Opcode::GetIndex => {
            // v1 `GetIndex a, b, c`: slot a = slot b [slot c]. v2
            // `LdaKeyedProperty r`: acc = r[acc] (key in acc).
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(OpcodeV2::LdaKeyedProperty, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::SetIndex => {
            // v1 `SetIndex a, b, c`: slot a [slot b] = slot c. v2
            // `StaKeyedProperty r0 r1`: r0[r1] = acc.
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::StaKeyedProperty,
                &[Operand::Reg(a), Operand::Reg(bx)],
            )?;
        }
        V1Opcode::DeleteProperty => {
            b.emit(
                OpcodeV2::DelNamedProperty,
                &[Operand::Reg(bx), Operand::Idx(c)],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::DeleteComputed => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(OpcodeV2::DelKeyedProperty, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }

        // --- object / array allocation ---
        V1Opcode::NewObject => {
            b.emit(OpcodeV2::CreateObject, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::NewArray => {
            // v1 `NewArray dst, len` — len is currently a hint only. v2
            // `CreateArray` creates an empty array; hinted length is a
            // future optimization.
            b.emit(OpcodeV2::CreateArray, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::NewRegExp => {
            b.emit(OpcodeV2::CreateRegExp, &[Operand::Idx(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::CreateArguments => {
            b.emit(OpcodeV2::CreateArguments, &[Operand::Imm(bx as i32)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::CreateRestParameters => {
            b.emit(OpcodeV2::CreateRestParameters, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::CreateEnumerableOwnKeys => {
            b.emit(OpcodeV2::CreateEnumerableOwnKeys, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }

        // --- iteration ---
        V1Opcode::GetIterator => emit_unary_reg(b, OpcodeV2::GetIterator, a, bx)?,
        V1Opcode::GetAsyncIterator => emit_unary_reg(b, OpcodeV2::GetAsyncIterator, a, bx)?,
        V1Opcode::IteratorNext => {
            // v1: a=done_dst, b=value_dst, c=iter. v2: `IteratorNext Reg`
            // writes the value into acc; the done flag flows through a
            // secondary channel that Phase 3b will wire (probably a
            // `secondary_result` slot on the Frame). For Phase 2a.3 we
            // preserve the value hand-off but leave done_dst untouched.
            b.emit(OpcodeV2::IteratorNext, &[Operand::Reg(c)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(bx)])?;
        }
        V1Opcode::IteratorClose => {
            // v1 `IteratorClose iter` has no destination; close is
            // side-effectful. v2 `IteratorClose Reg` mirrors that.
            b.emit(OpcodeV2::IteratorClose, &[Operand::Reg(a)])?;
        }
        V1Opcode::GetPropertyIterator => emit_unary_reg(b, OpcodeV2::ForInEnumerate, a, bx)?,
        V1Opcode::PropertyIteratorNext => {
            b.emit(
                OpcodeV2::ForInNext,
                &[Operand::Reg(bx), Operand::Reg(c)],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::SpreadIntoArray => {
            // v1 `SpreadIntoArray dst_array, src_iterable`. v2
            // `SpreadIntoArray r` appends acc's spread into r.
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::SpreadIntoArray, &[Operand::Reg(a)])?;
        }
        V1Opcode::ArrayPush => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::ArrayPush, &[Operand::Reg(a)])?;
        }
        V1Opcode::CopyDataProperties => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::CopyDataProperties, &[Operand::Reg(a)])?;
        }

        // --- accessors on object literals / class bodies ---
        V1Opcode::DefineNamedGetter => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineNamedGetter,
                &[Operand::Reg(a), Operand::Idx(bx)],
            )?;
        }
        V1Opcode::DefineNamedSetter => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineNamedSetter,
                &[Operand::Reg(a), Operand::Idx(bx)],
            )?;
        }
        V1Opcode::DefineComputedGetter => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineComputedGetter,
                &[Operand::Reg(a), Operand::Reg(bx)],
            )?;
        }
        V1Opcode::DefineComputedSetter => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineComputedSetter,
                &[Operand::Reg(a), Operand::Reg(bx)],
            )?;
        }

        // --- class fields ---
        V1Opcode::DefineField => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineField,
                &[Operand::Reg(a), Operand::Idx(bx)],
            )?;
        }
        V1Opcode::DefineComputedField => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineComputedField,
                &[Operand::Reg(a), Operand::Reg(bx)],
            )?;
        }
        V1Opcode::RunClassFieldInitializer => {
            b.emit(OpcodeV2::RunClassFieldInitializer, &[Operand::Reg(a)])?;
        }
        V1Opcode::SetClassFieldInitializer => {
            b.emit(OpcodeV2::SetClassFieldInitializer, &[Operand::Reg(a)])?;
        }
        V1Opcode::AllocClassId => {
            b.emit(OpcodeV2::AllocClassId, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::CopyClassId => {
            b.emit(OpcodeV2::CopyClassId, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }

        // --- private fields ---
        V1Opcode::DefinePrivateField => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(
                OpcodeV2::DefinePrivateField,
                &[Operand::Reg(a), Operand::Idx(c)],
            )?;
        }
        V1Opcode::GetPrivateField => {
            b.emit(
                OpcodeV2::GetPrivateField,
                &[Operand::Reg(bx), Operand::Idx(c)],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::SetPrivateField => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(
                OpcodeV2::SetPrivateField,
                &[Operand::Reg(a), Operand::Idx(c)],
            )?;
        }
        V1Opcode::DefinePrivateMethod => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(
                OpcodeV2::DefinePrivateMethod,
                &[Operand::Reg(a), Operand::Idx(c)],
            )?;
        }
        V1Opcode::DefinePrivateGetter => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(
                OpcodeV2::DefinePrivateGetter,
                &[Operand::Reg(a), Operand::Idx(c)],
            )?;
        }
        V1Opcode::DefinePrivateSetter => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(
                OpcodeV2::DefinePrivateSetter,
                &[Operand::Reg(a), Operand::Idx(c)],
            )?;
        }
        V1Opcode::PushPrivateMethod => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(
                OpcodeV2::PushPrivateMethod,
                &[Operand::Reg(a), Operand::Idx(c)],
            )?;
        }
        V1Opcode::PushPrivateGetter => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(
                OpcodeV2::PushPrivateGetter,
                &[Operand::Reg(a), Operand::Idx(c)],
            )?;
        }
        V1Opcode::PushPrivateSetter => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(
                OpcodeV2::PushPrivateSetter,
                &[Operand::Reg(a), Operand::Idx(c)],
            )?;
        }
        V1Opcode::InPrivate => {
            b.emit(
                OpcodeV2::InPrivate,
                &[Operand::Reg(bx), Operand::Idx(c)],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }

        // --- class methods ---
        V1Opcode::DefineClassMethod => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineClassMethod,
                &[Operand::Reg(a), Operand::Idx(bx)],
            )?;
        }
        V1Opcode::DefineClassMethodComputed => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineClassMethodComputed,
                &[Operand::Reg(a), Operand::Reg(bx)],
            )?;
        }
        V1Opcode::DefineClassGetter => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineClassGetter,
                &[Operand::Reg(a), Operand::Idx(bx)],
            )?;
        }
        V1Opcode::DefineClassSetter => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineClassSetter,
                &[Operand::Reg(a), Operand::Idx(bx)],
            )?;
        }
        V1Opcode::DefineClassGetterComputed => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineClassGetterComputed,
                &[Operand::Reg(a), Operand::Reg(bx)],
            )?;
        }
        V1Opcode::DefineClassSetterComputed => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(c)])?;
            b.emit(
                OpcodeV2::DefineClassSetterComputed,
                &[Operand::Reg(a), Operand::Reg(bx)],
            )?;
        }

        // --- super ---
        V1Opcode::SetHomeObject => {
            b.emit(
                OpcodeV2::SetHomeObject,
                &[Operand::Reg(a), Operand::Reg(bx)],
            )?;
        }
        V1Opcode::GetSuperProperty => {
            b.emit(
                OpcodeV2::GetSuperProperty,
                &[Operand::Reg(bx), Operand::Idx(c)],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::GetSuperPropertyComputed => {
            b.emit(
                OpcodeV2::GetSuperPropertyComputed,
                &[Operand::Reg(bx), Operand::Reg(c)],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::SetSuperProperty => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            b.emit(
                OpcodeV2::SetSuperProperty,
                &[Operand::Reg(bx), Operand::Idx(c)],
            )?;
        }
        V1Opcode::SetSuperPropertyComputed => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            b.emit(
                OpcodeV2::SetSuperPropertyComputed,
                &[Operand::Reg(bx), Operand::Reg(c)],
            )?;
        }
        V1Opcode::ThrowConstAssign => {
            b.emit(OpcodeV2::ThrowConstAssign, &[])?;
        }
        V1Opcode::AssertNotHole => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            b.emit(OpcodeV2::AssertNotHole, &[])?;
        }
        V1Opcode::AssertConstructor => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(a)])?;
            b.emit(OpcodeV2::AssertConstructor, &[])?;
        }

        // --- generators / async ---
        V1Opcode::Yield => {
            // v1 `Yield dst, src`: yield `src`, resumed value into `dst`.
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::Yield, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::YieldStar => {
            b.emit(OpcodeV2::YieldStar, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::Await => {
            b.emit(OpcodeV2::Ldar, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::Await, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }

        // --- modules ---
        V1Opcode::DynamicImport => {
            b.emit(OpcodeV2::DynamicImport, &[Operand::Reg(bx)])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::ImportMeta => {
            b.emit(OpcodeV2::ImportMeta, &[])?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }

        // --- calls (require side-table lookup) ---
        V1Opcode::CallDirect
        | V1Opcode::CallClosure
        | V1Opcode::CallSpread
        | V1Opcode::CallSuper
        | V1Opcode::CallSuperForward
        | V1Opcode::CallSuperSpread
        | V1Opcode::CallEval
        | V1Opcode::TailCallClosure => {
            let func = function.ok_or(TranspileError::MissingFunctionContext {
                pc,
                opcode: instr.opcode(),
            })?;
            emit_call(b, pc, instr, func)?;
        }

        // Everything else — extended progressively.
        other => {
            return Err(TranspileError::Unsupported {
                pc,
                opcode: other,
            });
        }
    }

    Ok(())
}

/// `Lda r; Op r ; Star dst` pattern for unary ops whose v2 form is
/// `Op Reg` (i.e. they read the target register, produce into acc). Used
/// for `GetIterator`, `IteratorNext`, etc.
fn emit_unary_reg(
    b: &mut BytecodeBuilder,
    op: OpcodeV2,
    dst: u32,
    src: u32,
) -> Result<(), TranspileError> {
    b.emit(op, &[Operand::Reg(src)])?;
    b.emit(OpcodeV2::Star, &[Operand::Reg(dst)])?;
    Ok(())
}

/// Lower a v1 call-family opcode using the function's `CallTable` to
/// recover argument counts and receiver slots. This is the only v1 → v2
/// path that needs side-table context.
fn emit_call(
    b: &mut BytecodeBuilder,
    pc: u32,
    instr: V1Instruction,
    function: &Function,
) -> Result<(), TranspileError> {
    let a = u32::from(instr.a());
    let bx = u32::from(instr.b());
    let c = u32::from(instr.c());
    let pc_pc: ProgramCounter = pc;

    match instr.opcode() {
        V1Opcode::CallDirect => {
            let call = function.calls().get_direct(pc_pc).ok_or(
                TranspileError::MissingCallMetadata {
                    pc,
                    opcode: V1Opcode::CallDirect,
                },
            )?;
            b.emit(
                OpcodeV2::CallDirect,
                &[
                    Operand::Idx(call.callee().0),
                    Operand::RegList {
                        base: bx,
                        count: u32::from(call.argument_count()),
                    },
                ],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::CallClosure => {
            let call = function.calls().get_closure(pc_pc).ok_or(
                TranspileError::MissingCallMetadata {
                    pc,
                    opcode: V1Opcode::CallClosure,
                },
            )?;
            let argc = u32::from(call.argument_count());
            if let Some(recv) = call.receiver() {
                b.emit(
                    OpcodeV2::CallAnyReceiver,
                    &[
                        Operand::Reg(bx),
                        Operand::Reg(u32::from(recv.index())),
                        Operand::RegList { base: c, count: argc },
                    ],
                )?;
            } else {
                b.emit(
                    OpcodeV2::CallUndefinedReceiver,
                    &[
                        Operand::Reg(bx),
                        Operand::RegList { base: c, count: argc },
                    ],
                )?;
            }
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::CallSpread => {
            // v1 `CallSpread dst, callee, args_array_reg`. v2 uses
            // `CallSpread callee recv RegList{arr, 1}` with callee as
            // placeholder receiver. Dispatch layer resolves real recv.
            b.emit(
                OpcodeV2::CallSpread,
                &[
                    Operand::Reg(bx),
                    Operand::Reg(bx),
                    Operand::RegList { base: c, count: 1 },
                ],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::CallSuper => {
            // v1 `CallSuper dst, args_base_reg, argc_imm`.
            b.emit(
                OpcodeV2::CallSuper,
                &[Operand::RegList { base: bx, count: c }],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::CallSuperForward => {
            // Forward uses the current frame's argument window — the
            // dispatch layer supplies base/count; Phase 2a.3 emits a
            // placeholder zero-count call. Phase 3b replaces with the
            // real forward semantics.
            b.emit(
                OpcodeV2::CallSuper,
                &[Operand::RegList { base: a, count: 0 }],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::CallSuperSpread => {
            b.emit(
                OpcodeV2::CallSuperSpread,
                &[Operand::RegList { base: bx, count: 1 }],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::CallEval => {
            let call = function.calls().get_closure(pc_pc).ok_or(
                TranspileError::MissingCallMetadata {
                    pc,
                    opcode: V1Opcode::CallEval,
                },
            )?;
            let argc = u32::from(call.argument_count());
            b.emit(
                OpcodeV2::CallEval,
                &[
                    Operand::Reg(bx),
                    Operand::Reg(bx),
                    Operand::RegList { base: c, count: argc },
                ],
            )?;
            b.emit(OpcodeV2::Star, &[Operand::Reg(a)])?;
        }
        V1Opcode::TailCallClosure => {
            let call = function.calls().get_closure(pc_pc).ok_or(
                TranspileError::MissingCallMetadata {
                    pc,
                    opcode: V1Opcode::TailCallClosure,
                },
            )?;
            b.emit(
                OpcodeV2::TailCall,
                &[
                    Operand::Reg(a),
                    Operand::Reg(a),
                    Operand::RegList {
                        base: bx,
                        count: u32::from(call.argument_count()),
                    },
                ],
            )?;
        }
        _ => unreachable!("emit_call dispatched for a non-call opcode"),
    }
    Ok(())
}

fn emit_binop(
    b: &mut BytecodeBuilder,
    op: OpcodeV2,
    dst: u32,
    lhs: u32,
    rhs: u32,
) -> Result<(), TranspileError> {
    b.emit(OpcodeV2::Ldar, &[Operand::Reg(lhs)])?;
    b.emit(op, &[Operand::Reg(rhs)])?;
    b.emit(OpcodeV2::Star, &[Operand::Reg(dst)])?;
    Ok(())
}

fn emit_unary(
    b: &mut BytecodeBuilder,
    op: OpcodeV2,
    dst: u32,
    src: u32,
) -> Result<(), TranspileError> {
    b.emit(OpcodeV2::Ldar, &[Operand::Reg(src)])?;
    b.emit(op, &[])?;
    b.emit(OpcodeV2::Star, &[Operand::Reg(dst)])?;
    Ok(())
}

fn resolve_jump_target(
    pc: u32,
    offset: i32,
    total: usize,
) -> Result<usize, TranspileError> {
    // v1 convention: target = pc + 1 + offset (same as v2, but in
    // instruction counts, not byte counts).
    let target = i64::from(pc) + 1 + i64::from(offset);
    usize::try_from(target)
        .ok()
        .filter(|t| *t <= total)
        .ok_or(TranspileError::InvalidJumpTarget { pc, offset })
}
