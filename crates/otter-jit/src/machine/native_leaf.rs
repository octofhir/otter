//! Guarded static-native leaf contracts for Machine selection and verification.
//!
//! # Contents
//! - [`descriptor`] derives the call ABI from the VM's shared builtin registry.
//! - [`is_valid`] checks the complete physical call and exact-deopt contract.
//! - [`diagnostics`] attributes final lowering only when event capture is enabled.
//!
//! # Invariants
//! - Bootstrap identity and argument count come from one VM declaration.
//! - Proven Int32 abs/max/min use equivalent generated operations with Int32
//!   inputs and result; the abs overflow miss precedes the result definition.
//! - Leaves cannot allocate, collect, throw, or reenter JavaScript. Identity or
//!   operand misses deoptimize before the source call has any observable effect.
//! - The target specification owns the callee, argument, result, and clobber
//!   registers. Identity-guard scratch never overlaps the arguments.
//!
//! # See also
//! - [`otter_vm::jit_static_native`] — authoritative builtin declarations.

#[cfg(target_arch = "aarch64")]
pub(super) mod arm64;

use super::{
    CallDescriptor, CallEffects, CallTarget, ExceptionalEdge, MachineInstruction,
    MachineRepresentation, OperandConstraint, OperandPurpose, SafepointKind, TargetCapability,
    TargetClobberSet, TargetSpec,
};
use otter_vm::JitStaticNativeCall;

pub(super) fn supports_site(
    view: &otter_vm::JitCompileSnapshot,
    target: JitStaticNativeCall,
    argument_count: usize,
) -> bool {
    let Some(declaration) = otter_vm::jit_static_native::jit_leaf_builtin(target.leaf_stub_id)
    else {
        return false;
    };
    view.native_ref_byte != 0
        && argument_count == usize::from(declaration.argument_count)
        && otter_vm::runtime_stubs::leaf_no_alloc_stub2_by_id(target.leaf_stub_id)
            .is_some_and(|stub| stub.is_valid())
}

pub(super) fn supports_int32(stub: otter_vm::native_abi::RuntimeStubId) -> bool {
    use otter_vm::native_abi::{STUB_MATH_ABS_LEAF, STUB_MATH_MAX_LEAF, STUB_MATH_MIN_LEAF};
    [
        STUB_MATH_ABS_LEAF.id,
        STUB_MATH_MAX_LEAF.id,
        STUB_MATH_MIN_LEAF.id,
    ]
    .contains(&stub)
}

pub(super) fn descriptor(
    target_spec: &TargetSpec,
    target: JitStaticNativeCall,
    byte_pc: u32,
    representation: MachineRepresentation,
) -> Option<CallDescriptor> {
    let declaration = otter_vm::jit_static_native::jit_leaf_builtin(target.leaf_stub_id)?;
    if !target_spec.supports(TargetCapability::NativeLeaf)
        || !matches!(
            representation,
            MachineRepresentation::Tagged | MachineRepresentation::Int32
        )
        || (representation == MachineRepresentation::Int32 && !supports_int32(target.leaf_stub_id))
        || target.argument_count > 2
        || declaration.argument_count != target.argument_count
        || !otter_vm::runtime_stubs::leaf_no_alloc_stub2_by_id(target.leaf_stub_id)
            .is_some_and(|stub| stub.is_valid())
    {
        return None;
    }
    let mut clobbers = if representation == MachineRepresentation::Int32 {
        target_spec
            .clobbers(TargetClobberSet::NativeLeafInt32)
            .to_vec()
    } else {
        target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec()
    };
    clobbers.retain(|register| *register != target_spec.integer_result());
    Some(CallDescriptor {
        target: CallTarget::NativeLeaf { target, byte_pc },
        arguments: std::iter::once(MachineRepresentation::Tagged)
            .chain(std::iter::repeat_n(
                representation,
                usize::from(target.argument_count),
            ))
            .collect(),
        results: vec![representation],
        effects: CallEffects::READS_HEAP,
        clobbers,
        exceptional: ExceptionalEdge::None,
        safepoint: SafepointKind::None,
    })
}

pub(super) fn is_valid(
    target_spec: &TargetSpec,
    call: &CallDescriptor,
    instruction: &MachineInstruction,
) -> bool {
    let CallTarget::NativeLeaf { target, byte_pc } = call.target else {
        return false;
    };
    let [representation] = call.results.as_slice() else {
        return false;
    };
    if descriptor(target_spec, target, byte_pc, *representation).as_ref() != Some(call)
        || instruction.deopt.is_none()
        || instruction.safepoint.is_some()
    {
        return false;
    }
    let actual = instruction
        .operands
        .iter()
        .filter_map(|operand| match operand.purpose {
            OperandPurpose::Input | OperandPurpose::Output => {
                Some((operand.purpose, operand.constraint))
            }
            _ => None,
        });
    let input = |register| (OperandPurpose::Input, OperandConstraint::Fixed(register));
    let callee = target_spec.callee_register();
    let Some(arguments) = (1..=usize::from(target.argument_count))
        .map(|index| target_spec.integer_argument(index))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let expected = std::iter::once(input(callee))
        .chain(arguments.into_iter().map(input))
        .chain(std::iter::once((
            OperandPurpose::Output,
            OperandConstraint::Fixed(target_spec.integer_result()),
        )));
    actual.eq(expected)
}

#[cfg(target_arch = "aarch64")]
pub(super) fn diagnostics(
    view: &otter_vm::JitCompileSnapshot,
    sequence: &super::InstructionSequence,
) -> Box<[otter_vm::JitCompilerDiagnostic]> {
    use otter_vm::{
        JitStaticNativeCallLoweringOutcome as Outcome,
        JitStaticNativeCallLoweringRejectionReason as Rejection,
    };
    let mut sites = view.static_native_calls.iter().collect::<Vec<_>>();
    sites.sort_unstable_by_key(|(byte_pc, _)| **byte_pc);
    sites.into_iter().filter_map(|(&byte_pc, target)| {
        let instruction = view.instructions.iter().find(|instruction| instruction.byte_pc == byte_pc)?;
        let generated = sequence.call_descriptors().iter().any(|descriptor| matches!(
            descriptor.target, CallTarget::NativeLeaf { byte_pc: pc, .. } if pc == byte_pc
        ));
        let count_operand = match instruction.op(&view.code_block) {
            otter_bytecode::Op::Call => 2,
            otter_bytecode::Op::CallWithThis => 3,
            _ => return None,
        };
        let outcome = if generated {
            Outcome::Generated
        } else {
            let present = sequence.call_descriptors().iter().any(|descriptor| matches!(
                descriptor.target,
                CallTarget::Direct { byte_pc: pc, .. } | CallTarget::ColdCallExit { byte_pc: pc, .. }
                    if pc == byte_pc
            ));
            Outcome::Rejected { reason: if !present {
                Rejection::Eliminated
            } else if instruction.const_index(&view.code_block, count_operand) != Some(u32::from(target.argument_count)) {
                Rejection::ArityUnsupported
            } else {
                Rejection::LayoutUnsupported
            }}
        };
        Some(otter_vm::JitCompilerDiagnostic::StaticNativeCallLowered {
            instruction_pc: instruction.instruction_pc(&view.code_block), byte_pc,
            target: otter_vm::native_abi::runtime_stub_name(target.leaf_stub_id), outcome,
        })
    }).collect()
}
