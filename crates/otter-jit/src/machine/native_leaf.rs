//! Guarded static-native leaf contracts for Machine selection and verification.
//!
//! # Contents
//! - [`descriptor`] derives the call ABI from the VM's shared builtin registry.
//! - [`is_valid`] checks the complete physical call and exact-deopt contract.
//! - [`diagnostics`] attributes final lowering only when event capture is enabled.
//! - Method and resolved-call probes reuse declarations with result/hit values.
//! - [`supports_leaf_probe`] and [`leaf_probe_clobbers`] admit a guarded
//!   method's own declared no-allocation leaf as a direct call.
//!
//! # Invariants
//! - Bootstrap identity and argument count come from one VM declaration.
//! - Proven Int32 abs/max/min use equivalent generated operations with Int32
//!   inputs and result; abs overflow misses before any observable effect.
//! - Leaves cannot allocate, collect, throw, or reenter JavaScript. Static
//!   `Op::Call` leaves retain exact pre-call deoptimization on misses. Method
//!   and resolved `CallWithThis` probes select their committed cold sibling;
//!   resolved calls retain the already-loaded callee without repeating lookup.
//! - A leaf probe call publishes no safepoint, frame or root record; its
//!   tagged operands stay in argument registers across the call only because
//!   the entry cannot move them.
//! - The target specification owns the callee, argument, result, and clobber
//!   registers. Identity-guard scratch never overlaps the arguments.
//!
//! # See also
//! - [`otter_vm::jit_static_native`] — authoritative builtin declarations.
//! - `super::numeric::native_call_cfg` — shared method/resolved-call CFG.

#[cfg(target_arch = "aarch64")]
pub(super) mod arm64;
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;

use super::{
    CallDescriptor, CallEffects, CallTarget, ExceptionalEdge, MachineInstruction,
    MachineRepresentation, OperandConstraint, OperandPurpose, SafepointKind, TargetCapability,
    TargetClobberSet, TargetSpec,
};
use otter_vm::JitStaticNativeCall;

pub(crate) fn supports_site(
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
        && target.argument_count == declaration.argument_count
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

/// Whether a guarded method's declared entry can be called as a leaf probe.
///
/// The entry comes from the method snapshot, not the static-call registry: an
/// exotic receiver's builtin (a String or collection method) has no ordinary
/// static-call identity, only the guarded prototype slot it occupies.
pub(super) fn supports_leaf_probe(stub: otter_vm::native_abi::RuntimeStubId) -> bool {
    otter_vm::runtime_stubs::leaf_no_alloc_stub2_by_id(stub).is_some_and(|leaf| leaf.is_valid())
}

/// A leaf probe is an ordinary scalar call except for its fixed boxed result.
pub(super) fn leaf_probe_clobbers(target_spec: &TargetSpec) -> Vec<super::PhysicalRegister> {
    let mut clobbers = target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec();
    clobbers.retain(|register| *register != target_spec.integer_result());
    clobbers
}

/// Scratch for the pure method probe, including its fixed Math operands.
pub(super) fn method_math_clobbers(target_spec: &TargetSpec) -> Vec<super::PhysicalRegister> {
    let mut clobbers = target_spec
        .clobbers(TargetClobberSet::PropertyLoad)
        .to_vec();
    clobbers.extend_from_slice(target_spec.clobbers(TargetClobberSet::NativeLeafInt32));
    clobbers.extend((1..=2).filter_map(|index| target_spec.integer_argument(index)));
    clobbers.sort_unstable();
    clobbers.dedup();
    clobbers.retain(|register| *register != target_spec.integer_result());
    clobbers
}

/// Verify pure native-method probes before allocation; neither can side-exit.
pub(super) fn method_probe_is_valid(
    target_spec: &TargetSpec,
    instruction: &MachineInstruction,
    representations: &[MachineRepresentation],
) -> bool {
    if !instruction.exits.is_empty() || instruction.safepoint.is_some() {
        return false;
    }
    let representation =
        |operand: &super::MachineOperand| representations[operand.value.0 as usize];
    match instruction.opcode {
        super::MachineOpcode::NativeLeafIdentity { .. } => {
            let [callee, active, hit] = instruction.operands.as_slice() else {
                return false;
            };
            *callee == super::MachineOperand::location_input(callee.value)
                && representation(callee) == MachineRepresentation::Tagged
                && *active == super::MachineOperand::location_input(active.value)
                && representation(active) == MachineRepresentation::Boolean
                && *hit == super::MachineOperand::register_output(hit.value)
                && representation(hit) == MachineRepresentation::Boolean
                && instruction.clobbers == target_spec.clobbers(TargetClobberSet::PropertyLoad)
        }
        super::MachineOpcode::NativeInt32Math { stub, .. } => {
            let Some(declaration) = otter_vm::jit_static_native::jit_leaf_builtin(stub) else {
                return false;
            };
            let count = usize::from(declaration.argument_count);
            if !target_spec.supports(TargetCapability::NativeLeaf)
                || !supports_int32(stub)
                || instruction.operands.len() != count + 3
            {
                return false;
            }
            let (arguments, tail) = instruction.operands.split_at(count);
            let [active, result, hit] = tail else {
                return false;
            };
            arguments.iter().enumerate().all(|(index, operand)| {
                target_spec
                    .integer_argument(index + 1)
                    .is_some_and(|register| {
                        *operand
                            == super::MachineOperand::fixed_register_input(operand.value, register)
                    })
                    && representation(operand) == MachineRepresentation::Int32
            }) && *active == super::MachineOperand::location_input(active.value)
                && representation(active) == MachineRepresentation::Boolean
                && *result
                    == super::MachineOperand::fixed_register_output(
                        result.value,
                        target_spec.integer_result(),
                    )
                && representation(result) == MachineRepresentation::Int32
                && *hit == super::MachineOperand::register_output(hit.value)
                && representation(hit) == MachineRepresentation::Boolean
                && instruction.clobbers == method_math_clobbers(target_spec)
        }
        super::MachineOpcode::NativeLeafProbe { stub, .. } => {
            let Some(words) = instruction.operands.len().checked_sub(3) else {
                return false;
            };
            if !target_spec.supports(TargetCapability::NativeLeaf)
                || !supports_leaf_probe(stub)
                || !(1..=2).contains(&words)
            {
                return false;
            }
            let (operands, tail) = instruction.operands.split_at(words);
            let [active, result, hit] = tail else {
                return false;
            };
            operands.iter().enumerate().all(|(index, operand)| {
                target_spec
                    .integer_argument(index + 1)
                    .is_some_and(|register| {
                        *operand
                            == super::MachineOperand::fixed_register_input(operand.value, register)
                    })
                    && representation(operand) == MachineRepresentation::Tagged
            }) && *active == super::MachineOperand::location_input(active.value)
                && representation(active) == MachineRepresentation::Boolean
                && *result
                    == super::MachineOperand::fixed_register_output(
                        result.value,
                        target_spec.integer_result(),
                    )
                && representation(result) == MachineRepresentation::Tagged
                && *hit == super::MachineOperand::register_output(hit.value)
                && representation(hit) == MachineRepresentation::Boolean
                && instruction.clobbers == leaf_probe_clobbers(target_spec)
        }
        _ => false,
    }
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
        || instruction.exits.is_empty()
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
        )) || sequence.instructions().iter().any(|instruction| matches!(
            instruction.opcode,
            super::MachineOpcode::NativeInt32Math { byte_pc: pc, stub }
                if pc == byte_pc && stub == target.leaf_stub_id
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{MachineOpcode, MachineOperand, MachineValue, SafepointId};
    use otter_vm::native_abi::{STUB_MATH_ABS_LEAF, STUB_MATH_MAX_LEAF, STUB_MATH_MIN_LEAF};

    fn identity(target: &TargetSpec) -> (MachineInstruction, Vec<MachineRepresentation>) {
        let mut instruction = MachineInstruction::plain(
            MachineOpcode::NativeLeafIdentity {
                builtin_native_ref: 17,
                byte_pc: 4,
            },
            vec![
                MachineOperand::location_input(MachineValue(0)),
                MachineOperand::location_input(MachineValue(1)),
                MachineOperand::register_output(MachineValue(2)),
            ],
        );
        instruction.clobbers = target.clobbers(TargetClobberSet::PropertyLoad).to_vec();
        (
            instruction,
            vec![
                MachineRepresentation::Tagged,
                MachineRepresentation::Boolean,
                MachineRepresentation::Boolean,
            ],
        )
    }

    fn math(
        target: &TargetSpec,
        stub: otter_vm::native_abi::RuntimeStubId,
    ) -> (MachineInstruction, Vec<MachineRepresentation>) {
        let count = usize::from(
            otter_vm::jit_static_native::jit_leaf_builtin(stub)
                .unwrap()
                .argument_count,
        );
        let mut representations = vec![MachineRepresentation::Int32; count];
        representations.extend([
            MachineRepresentation::Boolean,
            MachineRepresentation::Int32,
            MachineRepresentation::Boolean,
        ]);
        let mut operands = (0..count)
            .map(|index| {
                MachineOperand::fixed_register_input(
                    MachineValue(index as u32),
                    target.integer_argument(index + 1).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        operands.extend([
            MachineOperand::location_input(MachineValue(count as u32)),
            MachineOperand::fixed_register_output(
                MachineValue(count as u32 + 1),
                target.integer_result(),
            ),
            MachineOperand::register_output(MachineValue(count as u32 + 2)),
        ]);
        let mut instruction = MachineInstruction::plain(
            MachineOpcode::NativeInt32Math { stub, byte_pc: 4 },
            operands,
        );
        instruction.clobbers = method_math_clobbers(target);
        (instruction, representations)
    }

    #[test]
    fn method_identity_probe_rejects_wrong_types_outputs_and_safepoints() {
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            let (instruction, representations) = identity(&target);
            assert!(method_probe_is_valid(
                &target,
                &instruction,
                &representations
            ));
            let effects = instruction.opcode.effects();
            assert!(
                !effects.allocates && !effects.safepoint && !effects.reentrant && !effects.throws
            );
            for (index, invalid_type) in [
                (0, MachineRepresentation::Int32),
                (1, MachineRepresentation::Tagged),
                (2, MachineRepresentation::Tagged),
            ] {
                let mut wrong = representations.clone();
                wrong[index] = invalid_type;
                assert!(!method_probe_is_valid(&target, &instruction, &wrong));
            }
            let mut wrong = instruction.clone();
            wrong.operands[2] = MachineOperand::location_input(MachineValue(2));
            assert!(!method_probe_is_valid(&target, &wrong, &representations));
            let mut wrong = instruction.clone();
            wrong.safepoint = Some(SafepointId(0));
            assert!(!method_probe_is_valid(&target, &wrong, &representations));
            let mut wrong = instruction.clone();
            wrong.exits = Box::new([crate::machine::MachineExit {
                id: crate::machine::DeoptId(0),
                reason: otter_vm::native_abi::ExitReason::IdentityGuard,
                action: otter_vm::native_abi::ExitAction::Recompile,
            }]);
            assert!(!method_probe_is_valid(&target, &wrong, &representations));
            let mut wrong = instruction;
            wrong.clobbers.pop();
            assert!(!method_probe_is_valid(&target, &wrong, &representations));
        }
    }

    fn leaf_probe(
        target: &TargetSpec,
        words: usize,
    ) -> (MachineInstruction, Vec<MachineRepresentation>) {
        let mut representations = vec![MachineRepresentation::Tagged; words];
        representations.extend([
            MachineRepresentation::Boolean,
            MachineRepresentation::Tagged,
            MachineRepresentation::Boolean,
        ]);
        let mut operands = (0..words)
            .map(|index| {
                MachineOperand::fixed_register_input(
                    MachineValue(index as u32),
                    target.integer_argument(index + 1).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        operands.extend([
            MachineOperand::location_input(MachineValue(words as u32)),
            MachineOperand::fixed_register_output(
                MachineValue(words as u32 + 1),
                target.integer_result(),
            ),
            MachineOperand::register_output(MachineValue(words as u32 + 2)),
        ]);
        let mut instruction = MachineInstruction::plain(
            MachineOpcode::NativeLeafProbe {
                stub: otter_vm::native_abi::STUB_STRING_INDEX_OF_LEAF.id,
                byte_pc: 4,
            },
            operands,
        );
        instruction.clobbers = leaf_probe_clobbers(target);
        (instruction, representations)
    }

    #[test]
    fn leaf_probes_require_a_declared_leaf_and_complete_call_contract() {
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            for words in 1..=2 {
                let (instruction, representations) = leaf_probe(&target, words);
                assert!(method_probe_is_valid(
                    &target,
                    &instruction,
                    &representations
                ));
                let effects = instruction.opcode.effects();
                assert!(effects.writes.is_empty());
                assert!(
                    !effects.allocates
                        && !effects.safepoint
                        && !effects.reentrant
                        && !effects.throws
                );
                assert!(!leaf_probe_clobbers(&target).contains(&target.integer_result()));
                for index in 0..representations.len() {
                    let mut wrong = representations.clone();
                    wrong[index] = if representations[index] == MachineRepresentation::Tagged {
                        MachineRepresentation::Int32
                    } else {
                        MachineRepresentation::Tagged
                    };
                    assert!(!method_probe_is_valid(&target, &instruction, &wrong));
                }
                let mut wrong = instruction.clone();
                wrong.operands[0] = MachineOperand::register_input(MachineValue(0));
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
                let mut wrong = instruction.clone();
                wrong.opcode = MachineOpcode::NativeLeafProbe {
                    stub: otter_vm::native_abi::STUB_ARRAY_PUSH_ALLOC.id,
                    byte_pc: 4,
                };
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
                let mut wrong = instruction.clone();
                wrong.safepoint = Some(SafepointId(0));
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
                let mut wrong = instruction;
                wrong.clobbers.pop();
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
            }
            let (mut three, mut representations) = leaf_probe(&target, 2);
            three.operands.insert(
                0,
                MachineOperand::fixed_register_input(
                    MachineValue(9),
                    target.integer_argument(3).unwrap(),
                ),
            );
            representations.push(MachineRepresentation::Tagged);
            assert!(!method_probe_is_valid(&target, &three, &representations));
        }
    }

    #[test]
    fn method_math_probes_require_complete_fixed_int32_contract() {
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            for stub in [
                STUB_MATH_ABS_LEAF.id,
                STUB_MATH_MAX_LEAF.id,
                STUB_MATH_MIN_LEAF.id,
            ] {
                let (instruction, representations) = math(&target, stub);
                assert!(method_probe_is_valid(
                    &target,
                    &instruction,
                    &representations
                ));
                let effects = instruction.opcode.effects();
                assert!(effects.reads.is_empty() && effects.writes.is_empty());
                assert!(
                    !effects.allocates
                        && !effects.safepoint
                        && !effects.reentrant
                        && !effects.throws
                );
                let count = instruction.operands.len() - 3;
                let mut wrong = instruction.clone();
                wrong.operands.remove(0);
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
                let mut wrong = instruction.clone();
                wrong.operands[0] = MachineOperand::register_input(MachineValue(0));
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
                let mut wrong = instruction.clone();
                wrong.operands[count + 1] =
                    MachineOperand::register_output(MachineValue(count as u32 + 1));
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
                for index in 0..representations.len() {
                    let mut wrong = representations.clone();
                    wrong[index] = MachineRepresentation::Tagged;
                    assert!(!method_probe_is_valid(&target, &instruction, &wrong));
                }
                let mut wrong = instruction.clone();
                wrong.safepoint = Some(SafepointId(0));
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
                let mut wrong = instruction.clone();
                wrong.exits = Box::new([crate::machine::MachineExit {
                    id: crate::machine::DeoptId(0),
                    reason: otter_vm::native_abi::ExitReason::IdentityGuard,
                    action: otter_vm::native_abi::ExitAction::Recompile,
                }]);
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
                let mut wrong = instruction.clone();
                wrong.clobbers.pop();
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
                let mut wrong = instruction;
                wrong.opcode = MachineOpcode::NativeInt32Math {
                    stub: otter_vm::native_abi::STUB_STRING_CHAR_CODE_AT_LEAF.id,
                    byte_pc: 4,
                };
                assert!(!method_probe_is_valid(&target, &wrong, &representations));
            }
        }
    }
}
