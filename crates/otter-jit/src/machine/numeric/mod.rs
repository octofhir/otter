//! Production scalar-function lowering through the shared Machine IR pipeline.
//!
//! # Contents
//! - `hir` — typed scalar semantic graph with direct captured-binding reads,
//!   guarded property and element accesses, typed array construction, explicit
//!   reentrant calls, and catch landing pads.
//! - `frame_state` — source-owned activation chains and complete SSA liveness.
//! - `inlining` — guarded scalar callee CFG splicing before selection/allocation.
//! - `property_cfg` — explicit named-property probe/cold/status/landing/join blocks.
//! - `boxed_arithmetic` — use-demand relaxation of tagged immediate arithmetic.
//! - `arm64` — allocation-driven AArch64 emission.
//! - Derived-this committed operations split into generated and cold CFG
//!   siblings before allocation, sharing one SSA result and exception contract.
//! - Literal allocation publishes arbitrary boxed-value spans and precise roots
//!   through the same committed boundary as Template; holes retain their tag.
//! - Static-native leaves share VM declarations and exact pre-call deopt state;
//!   fixed ABI operands and call clobbers remain visible before allocation.
//! - [`try_compile`] — the sole production optimizing-tier entry.
//!
//! # See also
//! - [`crate::machine::TargetSpec`] — immutable target input to selection and allocation.
//! - [`crate::optimizing`] — tier policy and the production AArch64 target choice.
//!
//! # Invariants
//! - Bytecode is inspected only while building HIR; Machine IR and the emitter
//!   contain no bytecode operations.
//! - Root parameter guards bail at logical PC zero before observable effects.
//!   Spliced constructor guards retain their already allocated receiver.
//! - Constructor field programs are owned by the source HIR node and selected
//!   against its innermost frame, independently of the caller snapshot.
//! - Machine locations, edits, and frame size come only from regalloc2 output.
//! - Reducible loop headers outside active exception regions publish one
//!   representation-checked OSR trampoline that fills only live block
//!   parameters and never mutates the VM window. A protected header remains in
//!   the Machine body but cannot be entered without its materialized handler
//!   stack.
//! - Empty arithmetic feedback keeps tagged inputs and selects guarded Number
//!   operations; it never becomes an unconditional exit. Heterogeneous phi
//!   edges perform only lossless numeric widening or scalar boxing in explicit
//!   split blocks, after the exact backedge poll when applicable.
//! - Every representation-changing CFG edge is split. Lossless widening or
//!   boxing executes after any exact backedge poll and before the successor's
//!   phi moves; deopt state retains the original HIR values.
//! - Settled element accesses consume late allocator locations and perform no
//!   allocation or reentry on the generated hit. Ordinary packed-double arrays
//!   keep payload and scalar index unboxed through an exact
//!   Float64-to-Uint32 index guard; every miss remains a pre-effect deopt.
//!   Other prepared families retain an allocation-free fast index. Tagged and
//!   Number indices are already exact rooted Values; raw Int32/Uint32 indices
//!   remain in allocator-owned late homes and box only inside the cold sibling.
//!   Any receiver, index, bounds, layout, or hole miss calls the same canonical
//!   reentrant boxed-value boundary selected for missing direct metadata, with
//!   precise moving roots and effect-once completion. Tagged and generic
//!   committed accesses inside local catch regions remain materialized until
//!   Machine committed-throw landing is explicit.
//! - Named-load probes consume tagged late locations and return payload, hit
//!   and stable IC-address SSA values without a call or safepoint. Misses enter
//!   an explicit rooted committed pair call; Success joins, Throw enters the
//!   local catch or propagates, and Fatal exits. No property effect replays.
//!   Named stores use the same explicit cold CFG and return hit/address values
//!   from their non-reentrant generated commit. A proven non-cell value omits
//!   the value barrier; the transition-shape barrier retains its leaf clobbers.
//! - Every schema-owned binding read, write, and delete expands before
//!   allocation into explicit guard/hit/cold/status/join control. Stable
//!   captured cells and prepared global lexical/object slots read or write on
//!   the generated sibling; writes perform the generated barrier. A missing
//!   layout proof, TDZ, const violation, accessor/Proxy path, or unresolved name
//!   enters one rooted committed cold call and never deoptimizes or replays.
//!   Dynamic and eval-shadowed bindings deliberately keep only that cold path.
//! - Eagerly prepared string literals lower to one symbolic stable-cell
//!   relocation and tagged load. No moving string handle, safepoint, deopt
//!   state, or runtime-fill boundary survives selection.
//! - Tagged loose equality against a static nullish literal classifies
//!   immediates and ordinary cells directly and exits before its Boolean
//!   definition only for a native-function cell, the sole HTMLDDA carrier,
//!   without a generated call.
//! - Tagged truthiness expands to a no-call probe and explicit cold leaf before
//!   allocation. Primitive cells and HTMLDDA candidates retain canonical VM
//!   decisions; both paths join through ordinary Boolean SSA parameters.
//! - Allocating calls save every live tagged value from its exact late-use
//!   location into the frame's collector-visible root area and reload it after
//!   moving GC; no interpreter-window shuttle or emitter-local map exists.
//! - Zero-argument and one-Int32-argument `ArrayConstruct` operations call the
//!   stack-owned allocating boundary directly with fixed ABI registers. Wider
//!   or non-Int32 forms remain outside this pipeline.
//! - Complete one-to-four-target guarded method chains, zero-candidate attempted
//!   methods, monomorphic plain calls, explicit-receiver calls (one
//!   identity-guarded candidate or the generic value call, which an attempted
//!   polymorphic plain call also takes with an `undefined` receiver), and
//!   fixed/spread base/derived/super construction share one typed descriptor
//!   and generated linkage emitter.
//!   Method arity is independent of guarded target count. Every method's
//!   receiver-plus-argument packet fits one frame-wide untraced raw window used
//!   only by its canonical final miss. A never-attempted unplanned plain/method
//!   call selects a pure, root-free, safepoint-free exact deopt boundary.
//!   Spread lowering consumes the compiler-created dense argument array;
//!   eligible callees cannot observe discarded arguments through rest or the
//!   `arguments` object.
//!   Fixed committed boxed-value calls inside supported catch regions own
//!   explicit exceptional CFG successors. Generated JavaScript-call linkage
//!   remains outside local catches until it returns a pure exception value
//!   instead of a pending VM exception side channel.

mod arm64;
mod boxed_arithmetic;
mod constructor_effects;
mod element_cfg;
mod frame_state;
mod hir;
mod inline_reentry;
mod inlining;
mod property_cfg;
mod semantics;

use otter_vm::{
    JitArtifactFileName, JitCompileSnapshot,
    deopt::{DeoptExitDescriptor, DeoptRuntime},
    native_abi::{
        ExitAction, ExitReason, STUB_ARRAY_CONSTRUCT_ALLOC, STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW,
        STUB_JIT_BACKEDGE_POLL, STUB_JIT_CALL_METHOD_VALUE, STUB_JIT_CALL_WITH_THIS_VALUE,
        STUB_JIT_CONSTRUCT_VALUE, STUB_JIT_COPY_SPREAD_ARGUMENTS, STUB_JIT_DEOPT_STACK_CALL,
        STUB_JIT_DEOPT_WRITEBACK, STUB_JIT_DERIVED_CONSTRUCT_RESULT, STUB_JIT_INITIALIZE_UPVALUES,
        STUB_JIT_PREPARE_BASE_CONSTRUCT, STUB_JIT_RESOLVE_DIRECT_ENTRY,
        STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT,
    },
};
use std::collections::{BTreeMap, BTreeSet};

use self::hir::{
    NumericBindingTarget, NumericColdCallKind, NumericDirectCallArguments, NumericDirectCallKind,
    NumericDirectCallTarget, NumericElementAccess, NumericFramePoint, NumericFrameStatePurpose,
    NumericFunction, NumericNode, NumericPackedDoubleViewCachePlan, NumericTerminator, NumericType,
    NumericValue,
};
use self::semantics::CommittedValueOperation;
#[cfg(test)]
use super::is_explicit_committed_runtime_call;
use super::{
    CallDescriptor, CallEffects, CallTarget, ColdCallKind, ControlFlow, DeoptId,
    DirectCallArgumentMode, DirectCallCandidate, DirectCallKind, ExceptionalEdge,
    InstructionSequence, MachineBindingTarget, MachineBlock, MachineBlockData, MachineCallGuard,
    MachineExit, MachineInstruction, MachineInstructionId, MachineOpcode, MachineOperand,
    MachineOsrInput, MachineOsrType, MachineRepresentation, MachineValue,
    PACKED_DOUBLE_VIEW_CACHE_RAW_WORDS, PackedDoubleViewCacheClearReason, PhysicalRegister,
    SafepointId, SafepointKind, TargetCapability, TargetClobberSet, TargetSpec,
    binding_guard_clobbers, binding_hit_clobbers, binding_write_barrier_clobbers,
    lower_deopt_table, lower_safepoints,
};
use crate::{
    Unsupported,
    artifact::{ArtifactRequest, CodeMapCapture, CodeRegion, NativeCompileOutput, build_bundle},
    entry::TransitionTable,
    optimizing::{OptimizedCode, OptimizedMetadata},
};

/// Frame-wide untraced packet shared by calls and literal allocation.
///
/// Packed-double caches own the raw prefix. The packet starts immediately
/// after that prefix and is sized for the widest selected span descriptor.
/// Each descriptor owns its semantic operands; literal spans have no receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ValuePacketFrame {
    pub(super) raw_start: u16,
    pub(super) raw_words: u16,
}

#[derive(Debug, Clone, Copy)]
struct BindingSelectedValues {
    condition: MachineValue,
    owner: MachineValue,
    storage: MachineValue,
    hit_value: Option<MachineValue>,
    cold_payload: MachineValue,
    status: MachineValue,
}

pub(super) fn value_packet_frame(
    sequence: &InstructionSequence,
) -> Result<ValuePacketFrame, Unsupported> {
    let cache_words = u16::try_from(PACKED_DOUBLE_VIEW_CACHE_RAW_WORDS)
        .map_err(|_| Unsupported::OperandShape("scalar raw-cache word count"))?;
    let raw_start = u16::from(sequence.packed_double_view_cache_count())
        .checked_mul(cache_words)
        .ok_or(Unsupported::OperandShape(
            "scalar packed-double view-cache frame",
        ))?;
    let raw_words = sequence
        .call_descriptors()
        .iter()
        .filter(|descriptor| {
            matches!(
                &descriptor.target,
                CallTarget::Direct {
                    kind: DirectCallKind::Method
                        | DirectCallKind::CallWithThis
                        | DirectCallKind::Forward
                        | DirectCallKind::Construct,
                    ..
                } | CallTarget::LiteralAllocation { .. }
            )
        })
        .map(|descriptor| descriptor.arguments.len())
        .max()
        .map(u16::try_from)
        .transpose()
        .map_err(|_| Unsupported::OperandShape("scalar value-span packet frame"))?
        .unwrap_or(0);
    Ok(ValuePacketFrame {
        raw_start,
        raw_words,
    })
}

pub(crate) fn try_compile(
    target_spec: &TargetSpec,
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
    capture_events: bool,
    artifact_request: Option<ArtifactRequest>,
) -> Result<NativeCompileOutput<OptimizedCode>, Unsupported> {
    let mut hir = NumericFunction::build(view).map_err(|decline| match decline {
        hir::HirDecline::Structural(constraint) => Unsupported::OperandShape(constraint),
        hir::HirDecline::Instruction { op, constraint, .. } => {
            Unsupported::Constraint { op, constraint }
        }
    })?;
    let inline_diagnostics = inlining::splice(&mut hir, view, capture_events);
    let packed_double_view_caches = hir.plan_packed_double_view_caches(view);
    let sequence =
        select_with_packed_double_view_caches(target_spec, &hir, &packed_double_view_caches)
            .map_err(|_error| Unsupported::OperandShape("scalar HIR to Machine IR selection"))?;
    let load_property_sites = sequence
        .instructions()
        .iter()
        .filter(|instruction| {
            matches!(
                instruction.opcode,
                MachineOpcode::PropertySource { store: false, .. }
            )
        })
        .count();
    let store_property_sites = sequence
        .instructions()
        .iter()
        .filter(|instruction| {
            matches!(
                instruction.opcode,
                MachineOpcode::PropertySource { store: true, .. }
            )
        })
        .count();
    let mut load_ic_cells =
        vec![crate::entry::PropertySourceCell::default(); load_property_sites].into_boxed_slice();
    let mut store_ic_cells =
        vec![crate::entry::PropertySourceCell::default(); store_property_sites].into_boxed_slice();
    let allocation = sequence
        .allocate(target_spec)
        .map_err(|_| Unsupported::OperandShape("scalar Machine IR allocation"))?;
    let mut machine_safepoints = lower_safepoints(&sequence, &allocation)
        .map_err(|_| Unsupported::OperandShape("scalar Machine IR safepoint lowering"))?;
    inline_reentry::prepare_safepoints(view, &sequence, &mut machine_safepoints)?;
    let parameter_prefix_entry = machine_safepoints.is_empty()
        && !hir
            .frame_states
            .iter()
            .any(|state| matches!(state.point, NumericFramePoint::Backedge { .. }));
    let method_packet = value_packet_frame(&sequence)?;
    let inline_frame_words = arm64::inline_calls::frame_words(&sequence)?;
    let frame = target_spec
        .frame_layout(
            &allocation,
            machine_safepoints.root_slot_count(),
            method_packet
                .raw_start
                .checked_add(method_packet.raw_words)
                .and_then(|words| words.checked_add(inline_frame_words))
                .ok_or(Unsupported::OperandShape("scalar value-span packet frame"))?,
        )
        .map_err(|_| Unsupported::OperandShape("scalar Machine IR frame layout"))?;
    let (gpr_budget, fp_budget) = target_spec.deopt_register_budgets();
    let deopt_table = lower_deopt_table(
        &sequence,
        &allocation,
        frame,
        gpr_budget,
        fp_budget,
        sequence.frame_states(),
    )
    .map_err(|_| Unsupported::OperandShape("scalar Machine IR deopt lowering"))?;
    let exit_count = sequence
        .instructions()
        .iter()
        .flat_map(|instruction| instruction.exits.iter())
        .map(|exit| exit.id.0 as usize + 1)
        .max()
        .unwrap_or(0);
    let mut exits = vec![None; exit_count];
    for instruction in sequence.instructions() {
        let Some(state_id) = instruction.frame_state else {
            continue;
        };
        let state = sequence
            .frame_states()
            .get(state_id as usize)
            .ok_or(Unsupported::OperandShape("scalar logical frame state"))?;
        let resume_pcs: Box<[u32]> = state
            .frames
            .iter()
            .map(|frame| {
                frame_state::resume_pc(view, frame.function_id, frame.byte_pc)
                    .ok_or(Unsupported::OperandShape("scalar deopt source function/PC"))
            })
            .collect::<Result<_, _>>()?;
        for exit in &instruction.exits {
            let descriptor = DeoptExitDescriptor {
                state: state_id,
                reason: exit.reason,
                action: exit.action,
                resume_pcs: resume_pcs.clone(),
            };
            let slot = exits
                .get_mut(exit.id.0 as usize)
                .ok_or(Unsupported::OperandShape("scalar exit identity"))?;
            if slot.replace(descriptor).is_some() {
                return Err(Unsupported::OperandShape("duplicate scalar exit identity"));
            }
        }
    }
    let exits = exits
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(Unsupported::OperandShape("sparse scalar exit identities"))?;
    let deopt_runtime = Box::new(DeoptRuntime {
        table: deopt_table,
        exits: exits.into_boxed_slice(),
        gpr_budget,
    });
    let emission = arm64::emit(
        view,
        &sequence,
        &allocation,
        frame,
        &deopt_runtime,
        &machine_safepoints,
        transitions,
        transitions.entry(STUB_JIT_BACKEDGE_POLL),
        transitions.variadic_entry(STUB_JIT_DEOPT_WRITEBACK),
        transitions.entry(STUB_JIT_DEOPT_STACK_CALL),
        transitions.entry(STUB_JIT_RESOLVE_DIRECT_ENTRY),
        transitions.entry(STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT),
        transitions.entry(STUB_JIT_PREPARE_BASE_CONSTRUCT),
        transitions.entry(STUB_JIT_DERIVED_CONSTRUCT_RESULT),
        transitions.entry(STUB_JIT_COPY_SPREAD_ARGUMENTS),
        transitions.entry(STUB_JIT_INITIALIZE_UPVALUES),
        otter_vm::runtime_stubs::STRING_CONCAT_ALLOC
            .entry_addr()
            .ok_or(Unsupported::OperandShape(
                "scalar string concat runtime entry",
            ))? as u64,
        otter_vm::runtime_stubs::ARRAY_CONSTRUCT_ALLOC
            .entry_addr()
            .ok_or(Unsupported::OperandShape(
                "scalar ArrayConstruct runtime entry",
            ))? as u64,
        otter_vm::runtime_stubs::NUMBER_REM_F64_LEAF.entry_addr() as u64,
        otter_vm::runtime_stubs::NUMBER_POW_F64_LEAF.entry_addr() as u64,
        otter_vm::runtime_stubs::NUMBER_TO_INT32_F64_LEAF.entry_addr() as u64,
        otter_vm::runtime_stubs::STRICT_EQ_LEAF.entry_addr() as u64,
        otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.entry_addr() as u64,
        transitions.entry(STUB_JIT_CALL_METHOD_VALUE),
        transitions.entry(STUB_JIT_CALL_WITH_THIS_VALUE),
        transitions.entry(STUB_JIT_CONSTRUCT_VALUE),
        &mut load_ic_cells,
        &mut store_ic_cells,
        view.code_block.register_count,
        artifact_request.is_some(),
    )?;
    let machine_register_count = u8::try_from(allocation.used_register_count())
        .map_err(|_| Unsupported::OperandShape("scalar machine register count"))?;
    let safepoints = machine_safepoints.records().to_vec().into_boxed_slice();

    let arm64::Emission {
        code: emitted_code,
        generated_stack_frame_bytes,
        relocations,
        osr_entries,
        osr_regions,
        structural_regions,
    } = emission;

    let artifact = artifact_request.map(|request| {
        let mut tier_input = format!(
            "; backend=otter-machine-ir scalar-function\n; parameters={} registers={} blocks={} arithmetic-ops={}\n",
            hir.parameter_count,
            hir.register_count,
            hir.blocks.len(),
            hir.arithmetic_op_count
        );
        tier_input.push_str(&sequence.normalized());
        tier_input.push_str(&allocation.normalized());
        tier_input.push_str(&machine_safepoints.normalized());
        let mut code_map = CodeMapCapture::default();
        code_map.record(CodeRegion::structural(
            "machineScalarFunction",
            0,
            emitted_code.len(),
        ));
        for &(logical_pc, start, end) in &osr_regions {
            code_map.record_osr(logical_pc, start, end);
        }
        for &(kind, byte_pc, start, end) in &structural_regions {
            code_map.record(match byte_pc {
                Some(byte_pc) => CodeRegion::structural_at_byte_pc(kind, start, end, byte_pc),
                None => CodeRegion::structural(kind, start, end),
            });
        }
        build_bundle(
            request,
            view,
            code_object_id,
            &emitted_code,
            JitArtifactFileName::OptimizedIr,
            tier_input,
            code_map,
            relocations,
            Some(&deopt_runtime),
            &safepoints,
        )
    });

    let code = OptimizedCode::new(
        emitted_code,
        Some(generated_stack_frame_bytes),
        deopt_runtime,
        safepoints,
        osr_entries,
        Box::default(),
        load_ic_cells,
        store_ic_cells,
        OptimizedMetadata {
            code_object_id,
            function_id: view.code_block.id,
            param_count: view.code_block.param_count,
            register_count: view.code_block.register_count,
            parameter_prefix_entry,
            machine_register_count,
            allocator_spill_slot_count: allocation.spill_slots(),
            spill_slot_count: allocation
                .spill_slots()
                .saturating_add(u32::from(machine_safepoints.root_slot_count()))
                .saturating_add(u32::from(frame.raw_slots())),
        },
    );
    Ok(NativeCompileOutput {
        code,
        artifact,
        ir_node_count: u64::try_from(sequence.instructions().len()).unwrap_or(u64::MAX),
        diagnostics: if capture_events {
            inline_diagnostics
                .into_iter()
                .chain(super::native_leaf::diagnostics(view, &sequence))
                .collect()
        } else {
            Box::default()
        },
    })
}

fn select_with_packed_double_view_caches(
    target_spec: &TargetSpec,
    hir: &NumericFunction,
    packed_double_view_caches: &NumericPackedDoubleViewCachePlan,
) -> Result<InstructionSequence, super::VerificationError> {
    let mut representations = hir
        .nodes
        .iter()
        .map(|node| match node.value_type() {
            NumericType::Tagged => MachineRepresentation::Tagged,
            NumericType::Int32 => MachineRepresentation::Int32,
            NumericType::Uint32 => MachineRepresentation::Uint32,
            NumericType::Number => MachineRepresentation::Float64,
            NumericType::Boolean => MachineRepresentation::Boolean,
        })
        .collect::<Vec<_>>();
    let values = (0..hir.nodes.len())
        .map(|index| MachineValue(index as u32))
        .collect::<Vec<_>>();
    let mut tagged_parameters = vec![None; hir.parameter_count as usize];
    for (index, node) in hir.nodes.iter().enumerate() {
        let NumericNode::Parameter {
            register,
            value_type,
        } = node
        else {
            continue;
        };
        tagged_parameters[usize::from(*register)] = Some(if *value_type == NumericType::Tagged {
            values[index]
        } else {
            push_value(&mut representations, MachineRepresentation::Tagged)
        });
    }

    let selection_cfg = SelectionCfg::build(hir, packed_double_view_caches);
    let mut binding_values = BTreeMap::new();
    for (&block, selected) in &selection_cfg.bindings {
        let (node, target, _) = binding_site(hir, block)
            .ok_or(super::VerificationError::InvalidBlock(selected.join))?;
        let NumericNode::Binding { semantics, .. } = hir.nodes[node.0] else {
            unreachable!("binding site owns a binding node")
        };
        binding_values.insert(
            block,
            BindingSelectedValues {
                condition: push_value(&mut representations, MachineRepresentation::Boolean),
                owner: push_value(&mut representations, MachineRepresentation::Int64),
                storage: push_value(&mut representations, MachineRepresentation::Int64),
                hit_value: (target.is_some() && semantics.result_operand().is_some())
                    .then(|| push_value(&mut representations, MachineRepresentation::Tagged)),
                cold_payload: push_value(&mut representations, MachineRepresentation::Tagged),
                status: push_value(&mut representations, MachineRepresentation::NativeStatus),
            },
        );
    }
    let property_values = selection_cfg
        .properties
        .keys()
        .map(|&block| (block, property_cfg::Values::new(&mut representations)))
        .collect::<BTreeMap<_, _>>();
    let element_values = selection_cfg
        .elements
        .keys()
        .map(|&block| (block, element_cfg::Values::new(&mut representations)))
        .collect::<BTreeMap<_, _>>();
    let mut property_inputs = BTreeMap::new();
    let mut element_inputs = BTreeMap::new();
    let mut binding_inputs = BTreeMap::<usize, [Option<MachineValue>; 2]>::new();
    let mut instructions = Vec::with_capacity(hir.nodes.len() + hir.parameter_count as usize + 4);
    let mut call_descriptors = Vec::<CallDescriptor>::new();
    let mut committed_probes = BTreeMap::new();
    let mut next_safepoint = 0_u32;
    let mut blocks = Vec::with_capacity(selection_cfg.order.len());
    let frame_state_indices = hir
        .frame_states
        .iter()
        .enumerate()
        .map(|(index, state)| (state.point, index))
        .collect::<BTreeMap<_, _>>();
    let mut next_exit_id = 0_u32;
    let exit_specs = hir
        .frame_states
        .iter()
        .filter_map(|state| {
            let exits = frame_state_exits(hir, state, &mut next_exit_id);
            (!exits.is_empty()).then_some((state.point, exits))
        })
        .collect::<BTreeMap<_, _>>();
    for selected in &selection_cfg.order {
        let first = MachineInstructionId(instructions.len() as u32);
        let block_index = match *selected {
            SelectedBlock::Original(block_index) => block_index,
            SelectedBlock::PropertyHit(block_index)
            | SelectedBlock::PropertyCold(block_index)
            | SelectedBlock::PropertySuccess(block_index)
            | SelectedBlock::PropertyThrow(block_index)
            | SelectedBlock::PropertyFatal(block_index)
            | SelectedBlock::PropertyJoin(block_index) => {
                blocks.push(property_cfg::select_block(
                    target_spec,
                    *selected,
                    hir,
                    &selection_cfg,
                    block_index,
                    property_values[&block_index],
                    property_inputs[&block_index],
                    &values,
                    &mut representations,
                    &mut call_descriptors,
                    &mut next_safepoint,
                    &mut instructions,
                )?);
                continue;
            }
            SelectedBlock::ElementHit(block_index)
            | SelectedBlock::ElementCold(block_index)
            | SelectedBlock::ElementSuccess(block_index)
            | SelectedBlock::ElementThrow(block_index)
            | SelectedBlock::ElementFatal(block_index)
            | SelectedBlock::ElementJoin(block_index) => {
                blocks.push(element_cfg::select_block(
                    target_spec,
                    *selected,
                    hir,
                    &selection_cfg,
                    block_index,
                    element_values[&block_index],
                    element_inputs[&block_index],
                    &values,
                    &mut representations,
                    &mut call_descriptors,
                    &mut next_safepoint,
                    &mut instructions,
                )?);
                continue;
            }
            SelectedBlock::BindingHit(block_index) => {
                blocks.push(select_binding_hit_block(
                    target_spec,
                    hir,
                    &selection_cfg,
                    block_index,
                    binding_values[&block_index],
                    binding_inputs[&block_index],
                    &mut instructions,
                )?);
                continue;
            }
            SelectedBlock::BindingCold(block_index) => {
                blocks.push(select_binding_cold_block(
                    target_spec,
                    hir,
                    &selection_cfg,
                    block_index,
                    binding_values[&block_index],
                    binding_inputs[&block_index],
                    &values,
                    &mut representations,
                    &frame_state_indices,
                    &mut call_descriptors,
                    &mut next_safepoint,
                    &mut instructions,
                )?);
                continue;
            }
            SelectedBlock::BindingSuccess(block_index) => {
                blocks.push(select_binding_success_block(
                    hir,
                    &selection_cfg,
                    block_index,
                    binding_values[&block_index],
                    &mut instructions,
                )?);
                continue;
            }
            SelectedBlock::BindingThrow(block_index) => {
                blocks.push(select_binding_throw_block(
                    &selection_cfg,
                    block_index,
                    binding_values[&block_index],
                    &mut instructions,
                ));
                continue;
            }
            SelectedBlock::BindingFatal(block_index) => {
                blocks.push(select_binding_fatal_block(
                    &selection_cfg,
                    block_index,
                    &mut instructions,
                ));
                continue;
            }
            SelectedBlock::BindingJoin(block_index) => {
                blocks.push(select_binding_join_block(
                    hir,
                    &selection_cfg,
                    block_index,
                    &values,
                    &mut instructions,
                )?);
                continue;
            }
            SelectedBlock::SplitEdge {
                predecessor,
                edge,
                successor,
            } => {
                if let Some(source) = exceptional_hir_edge_source(hir, predecessor, edge) {
                    let descriptor_index = intern_call_descriptor(
                        &mut call_descriptors,
                        caught_throw_acknowledgement_descriptor(target_spec),
                    );
                    let mut acknowledgement = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        Vec::new(),
                    );
                    acknowledgement.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    acknowledgement.frame_state = Some(
                        u32::try_from(frame_state_indices[&NumericFramePoint::Node(source)])
                            .expect("bounded scalar frame-state count"),
                    );
                    instructions.push(acknowledgement);
                }
                if successor <= predecessor {
                    let point = NumericFramePoint::Backedge { predecessor, edge };
                    let state_index = frame_state_indices[&point];
                    let exits = exit_specs[&point].clone();
                    let mut poll =
                        MachineInstruction::plain(MachineOpcode::BackedgePoll, Vec::new());
                    attach_frame_state(hir, &values, state_index, exits, &mut poll);
                    poll.clobbers = target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec();
                    instructions.push(poll);
                }
                if packed_double_view_caches
                    .caches
                    .iter()
                    .any(|cache| cache.entry_edges.contains(&(predecessor, edge)))
                {
                    instructions.push(MachineInstruction::plain(
                        MachineOpcode::ClearPackedDoubleViewCaches(
                            PackedDoubleViewCacheClearReason::LoopEntry,
                        ),
                        Vec::new(),
                    ));
                }
                let property_exception = (property_cfg::exceptional(hir, predecessor)
                    == Some(edge))
                .then(|| property_cfg::site(hir, predecessor))
                .flatten();
                let binding_exception = binding_site(hir, predecessor)
                    .filter(|(_, _, exceptional)| *exceptional == Some(edge));
                let element_exception = (element_cfg::exceptional(hir, predecessor) == Some(edge))
                    .then(|| element_cfg::site(hir, predecessor))
                    .flatten();
                let successor_arguments = hir.blocks[predecessor].successor_arguments[edge]
                    .iter()
                    .zip(&hir.blocks[successor].parameters)
                    .map(|(&argument, &parameter)| {
                        if property_exception == Some(argument) {
                            return Ok(property_values[&predecessor].cold_payload);
                        }
                        if element_exception == Some(argument) {
                            return Ok(element_values[&predecessor].cold_payload);
                        }
                        if let Some((binding, _, _)) = binding_exception
                            && argument == binding
                        {
                            return Ok(binding_values[&predecessor].cold_payload);
                        }
                        select_edge_argument(
                            hir,
                            &values,
                            &mut representations,
                            &mut instructions,
                            argument,
                            parameter,
                            selection_cfg.originals[successor],
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let mut jump = MachineInstruction::plain(MachineOpcode::Jump, Vec::new());
                jump.control = ControlFlow::Branch;
                instructions.push(jump);
                let end = MachineInstructionId(instructions.len() as u32);
                blocks.push(MachineBlockData {
                    first,
                    end,
                    predecessors: vec![if property_exception.is_some() {
                        selection_cfg.properties[&predecessor].cold
                    } else if element_exception.is_some() {
                        selection_cfg.elements[&predecessor].cold
                    } else if binding_exception.is_some() {
                        selection_cfg.bindings[&predecessor].cold
                    } else {
                        selection_cfg.normal_exit(predecessor)
                    }],
                    successors: vec![selection_cfg.originals[successor]],
                    parameters: Vec::new(),
                    successor_arguments: vec![successor_arguments],
                });
                continue;
            }
        };
        let block = &hir.blocks[block_index];
        if block_index == 0 {
            for (parameter, &tagged) in tagged_parameters.iter().enumerate() {
                let Some(tagged) = tagged else {
                    continue;
                };
                instructions.push(MachineInstruction::plain(
                    MachineOpcode::EntryValue(parameter as u16),
                    vec![MachineOperand::register_output(tagged)],
                ));
            }
        }
        if block.osr_entry_allowed
            && block
                .predecessors
                .iter()
                .any(|&predecessor| predecessor >= block_index)
        {
            debug_assert_eq!(block.parameters.len(), block.parameter_registers.len());
            let inputs = block
                .parameters
                .iter()
                .zip(&block.parameter_registers)
                .map(|(&parameter, &frame_register)| MachineOsrInput {
                    frame_register,
                    value_type: match hir.nodes[parameter.0].value_type() {
                        NumericType::Tagged => MachineOsrType::Tagged,
                        NumericType::Int32 => MachineOsrType::Int32,
                        NumericType::Uint32 => MachineOsrType::Uint32,
                        NumericType::Number => MachineOsrType::Float64,
                        NumericType::Boolean => MachineOsrType::Boolean,
                    },
                })
                .collect();
            instructions.push(MachineInstruction::plain(
                MachineOpcode::OsrEntry {
                    logical_pc: block.logical_pc,
                    inputs,
                },
                block
                    .parameters
                    .iter()
                    .map(|&parameter| {
                        MachineOperand::location_input(machine_value(&values, parameter))
                    })
                    .collect(),
            ));
        }
        let mut selected_binding_guard = false;
        for &node_value in &block.nodes {
            let result = values[node_value.0];
            let node = hir.nodes[node_value.0];
            if let NumericNode::InlineCallGuard {
                source,
                function_id,
                this_mode,
            } = node
            {
                let guarded = push_value(&mut representations, MachineRepresentation::Tagged);
                let mut guard = MachineInstruction::plain(
                    MachineOpcode::GuardCallTarget {
                        guard: MachineCallGuard::Plain {
                            function_id,
                            this_mode,
                        },
                    },
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(guarded),
                    ],
                );
                guard.clobbers = target_spec
                    .clobbers(TargetClobberSet::InlineCallGuard)
                    .to_vec();
                let point = NumericFramePoint::Node(node_value);
                let state_index = frame_state_indices[&point];
                attach_frame_state(
                    hir,
                    &values,
                    state_index,
                    exit_specs[&point].clone(),
                    &mut guard,
                );
                instructions.push(guard);
                let mut resolve = MachineInstruction::plain(
                    MachineOpcode::ResolveCallThis { this_mode },
                    vec![
                        MachineOperand::register_input(guarded),
                        MachineOperand::register_output(result),
                    ],
                );
                resolve.clobbers = target_spec
                    .clobbers(TargetClobberSet::InlineCallGuard)
                    .to_vec();
                instructions.push(resolve);
                continue;
            }
            if let NumericNode::ConstructorFieldStore {
                object,
                value,
                byte_pc,
            } = node
            {
                let object = tagged_call_argument(
                    hir,
                    &values,
                    &mut representations,
                    &mut instructions,
                    object,
                );
                let value = tagged_call_argument(
                    hir,
                    &values,
                    &mut representations,
                    &mut instructions,
                    value,
                );
                let (owner, transition) = hir
                    .constructor_field_sites
                    .get(&node_value)
                    .ok_or(super::VerificationError::OpcodeSignatureMismatch(first))?;
                let state_index = frame_state_indices[&NumericFramePoint::Node(node_value)];
                let source = hir.frame_states[state_index]
                    .frames
                    .last()
                    .ok_or(super::VerificationError::OpcodeSignatureMismatch(first))?;
                if source.function_id != *owner || source.byte_pc != byte_pc {
                    return Err(super::VerificationError::OpcodeSignatureMismatch(first));
                }
                constructor_effects::select(
                    target_spec,
                    hir,
                    object,
                    value,
                    transition,
                    state_index,
                    exit_specs[&NumericFramePoint::Node(node_value)].clone(),
                    &values,
                    &mut representations,
                    &mut instructions,
                )?;
                continue;
            }
            if matches!(
                node,
                NumericNode::PropertyLoad { .. } | NumericNode::PropertyStore { .. }
            ) {
                let (receiver, byte_pc, exotic_length, stored) = match node {
                    NumericNode::PropertyLoad {
                        receiver,
                        byte_pc,
                        exotic_length,
                        ..
                    } => (receiver, byte_pc, exotic_length, None),
                    NumericNode::PropertyStore {
                        receiver,
                        value,
                        byte_pc,
                    } => (receiver, byte_pc, false, Some(value)),
                    _ => unreachable!(),
                };
                if block.nodes.last().copied() != Some(node_value) {
                    return Err(super::VerificationError::OpcodeSignatureMismatch(first));
                }
                let receiver = tagged_call_argument(
                    hir,
                    &values,
                    &mut representations,
                    &mut instructions,
                    receiver,
                );
                let stored_type = stored.map(|value| hir.nodes[value.0].value_type());
                let stored = stored.map(|value| {
                    tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        value,
                    )
                });
                property_inputs.insert(block_index, (receiver, stored));
                let outputs = property_values[&block_index];
                let non_cell = stored_type.is_some_and(property_store_value_is_non_cell);
                let property = owned_property_site(hir, node_value, byte_pc)
                    .ok_or(super::VerificationError::OpcodeSignatureMismatch(first))?;
                let property_start = instructions.len();
                select_cache_ir_property_programs(
                    target_spec,
                    property,
                    receiver,
                    stored,
                    non_cell,
                    exotic_length,
                    outputs,
                    &mut representations,
                    &mut instructions,
                )?;
                let frame_state = u32::try_from(
                    *frame_state_indices
                        .get(&NumericFramePoint::Node(node_value))
                        .ok_or(super::VerificationError::OpcodeSignatureMismatch(first))?,
                )
                .map_err(|_| super::VerificationError::OpcodeSignatureMismatch(first))?;
                for instruction in &mut instructions[property_start..] {
                    if instruction.opcode.cache_ir_effects().is_some() {
                        instruction.frame_state = Some(frame_state);
                    }
                }
                let mut branch = MachineInstruction::plain(
                    MachineOpcode::BranchIf(true),
                    vec![MachineOperand::register_input(outputs.hit)],
                );
                branch.control = ControlFlow::Branch;
                instructions.push(branch);
                let selected = selection_cfg.properties[&block_index];
                let mut predecessors = incoming_edges(hir, block_index)
                    .into_iter()
                    .map(|(predecessor, edge)| {
                        selection_cfg
                            .split_edges
                            .get(&(predecessor, edge))
                            .copied()
                            .unwrap_or_else(|| selection_cfg.normal_exit(predecessor))
                    })
                    .collect::<Vec<_>>();
                predecessors.sort_unstable();
                blocks.push(MachineBlockData {
                    first,
                    end: MachineInstructionId(instructions.len() as u32),
                    predecessors,
                    successors: vec![selected.hit, selected.cold],
                    successor_arguments: vec![vec![], vec![]],
                    parameters: block
                        .parameters
                        .iter()
                        .map(|&value| machine_value(&values, value))
                        .collect(),
                });
                selected_binding_guard = true;
                break;
            }
            if matches!(
                node,
                NumericNode::ElementLoad { .. } | NumericNode::ElementStore { .. }
            ) {
                if block.nodes.last().copied() != Some(node_value) {
                    return Err(super::VerificationError::OpcodeSignatureMismatch(first));
                }
                let (receiver, index, stored, access, byte_pc) = match node {
                    NumericNode::ElementLoad {
                        receiver,
                        index,
                        access,
                        byte_pc,
                        ..
                    } => (receiver, index, None, access, byte_pc),
                    NumericNode::ElementStore {
                        receiver,
                        index,
                        value,
                        access,
                        byte_pc,
                        ..
                    } => (receiver, index, Some(value), access, byte_pc),
                    _ => unreachable!(),
                };
                let receiver = tagged_call_argument(
                    hir,
                    &values,
                    &mut representations,
                    &mut instructions,
                    receiver,
                );
                let index = tagged_call_argument(
                    hir,
                    &values,
                    &mut representations,
                    &mut instructions,
                    index,
                );
                let stored_fast = stored.map(|value| {
                    if access == Some(NumericElementAccess::PackedDouble)
                        && hir.nodes[value.0].value_type() == NumericType::Number
                    {
                        machine_value(&values, value)
                    } else {
                        tagged_call_argument(
                            hir,
                            &values,
                            &mut representations,
                            &mut instructions,
                            value,
                        )
                    }
                });
                let stored_tagged = stored.map(|value| {
                    tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        value,
                    )
                });
                let inputs = element_cfg::Inputs {
                    receiver,
                    index,
                    stored_fast,
                    stored_tagged,
                };
                element_inputs.insert(block_index, inputs);
                element_cfg::select_probe(
                    target_spec,
                    byte_pc,
                    access,
                    packed_double_view_caches.cache_for(node_value),
                    inputs,
                    element_values[&block_index],
                    &mut representations,
                    &mut instructions,
                )?;
                let mut branch = MachineInstruction::plain(
                    MachineOpcode::BranchIf(true),
                    vec![MachineOperand::register_input(
                        element_values[&block_index].hit,
                    )],
                );
                branch.control = ControlFlow::Branch;
                instructions.push(branch);
                let selected = selection_cfg.elements[&block_index];
                let mut predecessors = incoming_edges(hir, block_index)
                    .into_iter()
                    .map(|(predecessor, edge)| {
                        selection_cfg
                            .split_edges
                            .get(&(predecessor, edge))
                            .copied()
                            .unwrap_or_else(|| selection_cfg.normal_exit(predecessor))
                    })
                    .collect::<Vec<_>>();
                predecessors.sort_unstable();
                blocks.push(MachineBlockData {
                    first,
                    end: MachineInstructionId(instructions.len() as u32),
                    predecessors,
                    successors: vec![selected.hit, selected.cold],
                    successor_arguments: vec![vec![], vec![]],
                    parameters: block
                        .parameters
                        .iter()
                        .map(|&value| machine_value(&values, value))
                        .collect(),
                });
                selected_binding_guard = true;
                break;
            }
            if let NumericNode::Binding {
                semantics,
                inputs,
                target,
                byte_pc,
                ..
            } = node
            {
                if block.nodes.last().copied() != Some(node_value) {
                    return Err(super::VerificationError::OpcodeSignatureMismatch(
                        MachineInstructionId(instructions.len() as u32),
                    ));
                }
                let mut selected_inputs = [None, None];
                for (selected, input) in selected_inputs.iter_mut().zip(inputs) {
                    *selected = input.map(|input| {
                        tagged_call_argument(
                            hir,
                            &values,
                            &mut representations,
                            &mut instructions,
                            input,
                        )
                    });
                }
                binding_inputs.insert(block_index, selected_inputs);
                let selected_values = binding_values[&block_index];
                let target = target
                    .map(machine_binding_target)
                    .unwrap_or(MachineBindingTarget::Cold);
                let mut operands = vec![
                    MachineOperand::register_output(selected_values.condition),
                    MachineOperand::register_output(selected_values.owner),
                    MachineOperand::register_output(selected_values.storage),
                ];
                operands.extend(
                    selected_inputs
                        .into_iter()
                        .flatten()
                        .map(MachineOperand::location_input),
                );
                let mut guard = MachineInstruction::plain(
                    MachineOpcode::BindingGuard {
                        byte_pc,
                        semantics,
                        target,
                    },
                    operands,
                );
                guard.clobbers = binding_guard_clobbers(target_spec);
                instructions.push(guard);
                let selected_blocks = selection_cfg.bindings[&block_index];
                let (terminator, successors) = if let Some(hit) = selected_blocks.hit {
                    (
                        MachineInstruction::plain(
                            MachineOpcode::BranchIf(true),
                            vec![MachineOperand::register_input(selected_values.condition)],
                        ),
                        vec![hit, selected_blocks.cold],
                    )
                } else {
                    (
                        MachineInstruction::plain(MachineOpcode::Jump, Vec::new()),
                        vec![selected_blocks.cold],
                    )
                };
                let mut terminator = terminator;
                terminator.control = ControlFlow::Branch;
                instructions.push(terminator);
                let end = MachineInstructionId(instructions.len() as u32);
                let mut predecessors = incoming_edges(hir, block_index)
                    .into_iter()
                    .map(|(predecessor, edge)| {
                        selection_cfg
                            .split_edges
                            .get(&(predecessor, edge))
                            .copied()
                            .unwrap_or_else(|| selection_cfg.normal_exit(predecessor))
                    })
                    .collect::<Vec<_>>();
                predecessors.sort_unstable();
                blocks.push(MachineBlockData {
                    first,
                    end,
                    predecessors,
                    successor_arguments: vec![Vec::new(); successors.len()],
                    successors,
                    parameters: block
                        .parameters
                        .iter()
                        .map(|&value| machine_value(&values, value))
                        .collect(),
                });
                selected_binding_guard = true;
                break;
            }
            if let NumericNode::FloatToInt32(source) = node {
                if !target_spec.supports(TargetCapability::Float64ToInt32) {
                    return Err(super::VerificationError::OpcodeSignatureMismatch(
                        MachineInstructionId(instructions.len() as u32),
                    ));
                }
                let mut call = MachineInstruction::plain(
                    MachineOpcode::Float64ToInt32,
                    vec![
                        MachineOperand::fixed_register_input(
                            machine_value(&values, source),
                            target_spec
                                .float_argument(0)
                                .ok_or(super::VerificationError::InvalidEntry)?,
                        ),
                        MachineOperand::fixed_register_output(result, target_spec.integer_result()),
                    ],
                );
                call.clobbers = target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec();
                call.clobbers
                    .retain(|register| *register != target_spec.integer_result());
                instructions.push(call);
                continue;
            }
            if let NumericNode::Rem(left, right) | NumericNode::Pow(left, right) = node {
                let capability = if matches!(node, NumericNode::Rem(..)) {
                    TargetCapability::FloatRemainder
                } else {
                    TargetCapability::FloatPower
                };
                if !target_spec.supports(capability) {
                    return Err(super::VerificationError::OpcodeSignatureMismatch(
                        MachineInstructionId(instructions.len() as u32),
                    ));
                }
                let opcode = if matches!(node, NumericNode::Rem(..)) {
                    MachineOpcode::FloatRem
                } else {
                    MachineOpcode::FloatPow
                };
                let mut call = MachineInstruction::plain(
                    opcode,
                    vec![
                        MachineOperand::fixed_register_input(
                            machine_value(&values, left),
                            target_spec
                                .float_argument(0)
                                .ok_or(super::VerificationError::InvalidEntry)?,
                        ),
                        MachineOperand::fixed_register_input(
                            machine_value(&values, right),
                            target_spec
                                .float_argument(1)
                                .ok_or(super::VerificationError::InvalidEntry)?,
                        ),
                        MachineOperand::fixed_register_output(result, target_spec.float_result()),
                    ],
                );
                call.clobbers = target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec();
                call.clobbers
                    .retain(|register| *register != target_spec.float_result());
                instructions.push(call);
                continue;
            }
            let mut instruction = match node {
                NumericNode::Parameter {
                    register,
                    value_type,
                } => {
                    if value_type == NumericType::Tagged {
                        continue;
                    }
                    let tagged = tagged_parameters[usize::from(register)]
                        .expect("live HIR parameter has an entry value");
                    let opcode = match value_type {
                        NumericType::Int32 => MachineOpcode::DecodeInt32,
                        NumericType::Number => MachineOpcode::DecodeNumber,
                        NumericType::Tagged | NumericType::Uint32 | NumericType::Boolean => {
                            unreachable!("parameter inference emits only Int32 or Number")
                        }
                    };
                    let output = if value_type == NumericType::Int32 {
                        MachineOperand::register_reuse_output(result, 0)
                    } else {
                        MachineOperand::register_output(result)
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![MachineOperand::register_input(tagged), output],
                    )
                }
                NumericNode::InlineMethodGuard { source, target } => {
                    let invalid = || {
                        super::VerificationError::OpcodeSignatureMismatch(MachineInstructionId(
                            instructions.len() as u32,
                        ))
                    };
                    let target = hir
                        .direct_call_targets
                        .get(target as usize)
                        .ok_or_else(invalid)?;
                    let [candidate] = target.candidates.as_slice() else {
                        return Err(invalid());
                    };
                    let program = candidate.guard.as_ref().ok_or_else(invalid)?;
                    if target.kind != NumericDirectCallKind::Method
                        || program.method_fid != candidate.callee.plan.function_id
                    {
                        return Err(invalid());
                    }
                    let mut guard = MachineInstruction::plain(
                        MachineOpcode::GuardCallTarget {
                            guard: MachineCallGuard::Method(Box::new(program.clone())),
                        },
                        vec![
                            MachineOperand::register_input(machine_value(&values, source)),
                            MachineOperand::register_output(result),
                        ],
                    );
                    guard.clobbers = target_spec
                        .clobbers(TargetClobberSet::InlineMethodGuard)
                        .to_vec();
                    guard
                }
                NumericNode::InlineCallGuard { .. } => {
                    unreachable!("plain call guards select as guard plus this resolution")
                }
                NumericNode::InlineConstructGuard {
                    source,
                    function_id,
                } => {
                    let mut guard = MachineInstruction::plain(
                        MachineOpcode::GuardCallTarget {
                            guard: MachineCallGuard::Construct { function_id },
                        },
                        vec![
                            MachineOperand::register_input(machine_value(&values, source)),
                            MachineOperand::register_output(result),
                        ],
                    );
                    guard.clobbers = target_spec
                        .clobbers(TargetClobberSet::InlineCallGuard)
                        .to_vec();
                    guard
                }
                NumericNode::ConstructReceiver {
                    source,
                    plan,
                    byte_pc,
                } => {
                    let allocated = push_value(&mut representations, MachineRepresentation::Tagged);
                    let page = push_value(&mut representations, MachineRepresentation::Int64);
                    let mut probe = MachineInstruction::plain(
                        MachineOpcode::AllocateObject { plan, byte_pc },
                        vec![
                            MachineOperand::register_input(machine_value(&values, source)),
                            MachineOperand::register_output(allocated),
                            MachineOperand::register_output(page),
                        ],
                    );
                    probe.clobbers = target_spec
                        .clobbers(TargetClobberSet::ConstructReceiver)
                        .to_vec();
                    instructions.push(probe);
                    let hit = push_value(&mut representations, MachineRepresentation::Boolean);
                    let mut test = MachineInstruction::plain(
                        MachineOpcode::AllocationHit,
                        vec![
                            MachineOperand::register_input(allocated),
                            MachineOperand::register_output(hit),
                        ],
                    );
                    test.clobbers = target_spec
                        .clobbers(TargetClobberSet::ConstructReceiverHit)
                        .to_vec();
                    instructions.push(test);
                    let mut publish = MachineInstruction::plain(
                        MachineOpcode::PublishObject { byte_pc },
                        vec![
                            MachineOperand::register_input(machine_value(&values, source)),
                            MachineOperand::register_input(allocated),
                            MachineOperand::register_input(page),
                            MachineOperand::register_input(hit),
                            MachineOperand::register_output(result),
                        ],
                    );
                    publish.clobbers = target_spec
                        .clobbers(TargetClobberSet::ConstructReceiver)
                        .to_vec();
                    publish
                }
                NumericNode::ConstructReceiverHit(receiver) => {
                    let mut test = MachineInstruction::plain(
                        MachineOpcode::AllocationHit,
                        vec![
                            MachineOperand::register_input(machine_value(&values, receiver)),
                            MachineOperand::register_output(result),
                        ],
                    );
                    test.clobbers = target_spec
                        .clobbers(TargetClobberSet::ConstructReceiverHit)
                        .to_vec();
                    test
                }
                NumericNode::BaseConstructResult {
                    result: returned,
                    receiver,
                } => {
                    let mut select = MachineInstruction::plain(
                        MachineOpcode::BaseConstructResult,
                        vec![
                            MachineOperand::register_input(machine_value(&values, returned)),
                            MachineOperand::register_input(machine_value(&values, receiver)),
                            MachineOperand::register_output(result),
                        ],
                    );
                    select.clobbers = target_spec
                        .clobbers(TargetClobberSet::BaseConstructResult)
                        .to_vec();
                    select
                }
                NumericNode::BoxTagged(source) => MachineInstruction::plain(
                    match hir.nodes[source.0].value_type() {
                        NumericType::Int32 => MachineOpcode::BoxInt32,
                        NumericType::Uint32 => MachineOpcode::BoxUint32,
                        NumericType::Number => MachineOpcode::BoxNumber,
                        NumericType::Boolean => MachineOpcode::BoxBoolean,
                        NumericType::Tagged => {
                            return Err(super::VerificationError::OpcodeSignatureMismatch(
                                MachineInstructionId(instructions.len() as u32),
                            ));
                        }
                    },
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::TaggedToNumber(source) => MachineInstruction::plain(
                    MachineOpcode::DecodeNumber,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::TaggedToInt32(source) => MachineInstruction::plain(
                    MachineOpcode::DecodeInt32,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_reuse_output(result, 0),
                    ],
                ),
                NumericNode::BlockParameter(_) => continue,
                NumericNode::TaggedConstant(bits) => MachineInstruction::plain(
                    MachineOpcode::TaggedConstant(bits),
                    vec![MachineOperand::register_output(result)],
                ),
                NumericNode::This => MachineInstruction::plain(
                    MachineOpcode::EntryThis,
                    vec![MachineOperand::register_output(result)],
                ),
                NumericNode::ClassSuperConstructor(source) => {
                    let source = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        source,
                    );
                    let descriptor_index = intern_call_descriptor(
                        &mut call_descriptors,
                        class_super_constructor_descriptor(target_spec),
                    );
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        vec![
                            MachineOperand::register_input(source),
                            MachineOperand::register_output(result),
                        ],
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call
                }
                NumericNode::StringConstantCell { byte_pc, target } => {
                    let mut load = MachineInstruction::plain(
                        MachineOpcode::StringConstantCellLoad { byte_pc, target },
                        vec![MachineOperand::register_output(result)],
                    );
                    load.clobbers = string_constant_cell_load_clobbers(target_spec);
                    load
                }
                NumericNode::Binding { .. } => {
                    unreachable!("binding nodes select their explicit CFG before ordinary nodes")
                }
                NumericNode::LiteralAllocation {
                    target,
                    argument_start,
                    argument_count,
                    logical_pc,
                    byte_pc,
                } => {
                    let end = argument_start
                        .checked_add(argument_count)
                        .ok_or(super::VerificationError::InvalidEntry)?;
                    let inputs = hir
                        .operand_values
                        .get(argument_start as usize..end as usize)
                        .ok_or(super::VerificationError::InvalidEntry)?
                        .iter()
                        .copied()
                        .map(|input| {
                            tagged_call_argument(
                                hir,
                                &values,
                                &mut representations,
                                &mut instructions,
                                input,
                            )
                        })
                        .collect::<Vec<_>>();
                    let descriptor_index = intern_call_descriptor(
                        &mut call_descriptors,
                        CallDescriptor {
                            target: CallTarget::LiteralAllocation {
                                target,
                                logical_pc,
                                byte_pc,
                            },
                            arguments: vec![MachineRepresentation::Tagged; inputs.len()],
                            results: vec![MachineRepresentation::Tagged],
                            effects: CallEffects::READS_HEAP.union(CallEffects::WRITES_HEAP),
                            clobbers: target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec(),
                            exceptional: ExceptionalEdge::None,
                            safepoint: SafepointKind::Gc,
                        },
                    );
                    let mut operands = inputs
                        .iter()
                        .copied()
                        .map(MachineOperand::location_input)
                        .collect::<Vec<_>>();
                    operands.push(MachineOperand::register_output(result));
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        operands,
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call.safepoint = Some(super::SafepointId(next_safepoint));
                    next_safepoint += 1;
                    call
                }
                NumericNode::CommittedValue {
                    operation,
                    inputs,
                    logical_pc,
                    byte_pc,
                    exceptional_edge,
                } => {
                    if inputs[0].is_none() && inputs[1].is_some() {
                        return Err(super::VerificationError::OpcodeSignatureMismatch(
                            MachineInstructionId(instructions.len() as u32),
                        ));
                    }
                    let inputs = inputs
                        .into_iter()
                        .flatten()
                        .map(|input| {
                            tagged_call_argument(
                                hir,
                                &values,
                                &mut representations,
                                &mut instructions,
                                input,
                            )
                        })
                        .collect::<Vec<_>>();
                    let semantic_arity = u8::try_from(inputs.len()).map_err(|_| {
                        super::VerificationError::OpcodeSignatureMismatch(MachineInstructionId(
                            instructions.len() as u32,
                        ))
                    })?;
                    let descriptor_index = intern_call_descriptor(
                        &mut call_descriptors,
                        committed_value_descriptor(
                            target_spec,
                            operation,
                            logical_pc,
                            byte_pc,
                            semantic_arity,
                            exceptional_edge.map(|edge| {
                                let edge = usize::from(edge);
                                selection_cfg
                                    .split_edges
                                    .get(&(block_index, edge))
                                    .copied()
                                    .unwrap_or_else(|| {
                                        selection_cfg.originals[block.successors[edge]]
                                    })
                            }),
                        ),
                    );
                    let probe = match operation {
                        CommittedValueOperation::Scalar(
                            otter_vm::native_abi::ScalarValueOp::BindThisValue,
                        ) => Some(super::committed_probe::ProbeKind::DerivedThis),
                        CommittedValueOperation::ObjectProtocol(
                            otter_vm::native_abi::ObjectProtocolValueOp::LooseEqual,
                        ) => Some(super::committed_probe::ProbeKind::LooseEquality { equal: true }),
                        CommittedValueOperation::ObjectProtocol(
                            otter_vm::native_abi::ObjectProtocolValueOp::LooseNotEqual,
                        ) => {
                            Some(super::committed_probe::ProbeKind::LooseEquality { equal: false })
                        }
                        _ => None,
                    };
                    if let Some(probe) = probe
                        && committed_probes
                            .insert(descriptor_index as u32, probe)
                            .is_some_and(|old| old != probe)
                    {
                        return Err(super::VerificationError::InvalidEntry);
                    }
                    let mut operands = inputs
                        .iter()
                        .copied()
                        .map(MachineOperand::location_input)
                        .collect::<Vec<_>>();
                    operands.push(MachineOperand::register_output(result));
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        operands,
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call.safepoint = Some(super::SafepointId(next_safepoint));
                    next_safepoint = next_safepoint
                        .checked_add(1)
                        .expect("bounded scalar function safepoint count");
                    call
                }
                NumericNode::ConstructorFieldStore { .. } => {
                    unreachable!("constructor effects select before ordinary nodes")
                }
                NumericNode::ElementLoad { .. } | NumericNode::ElementStore { .. } => {
                    unreachable!("element nodes select their explicit CFG before ordinary nodes")
                }
                NumericNode::ArrayConstruct { length, byte_pc: _ } => {
                    if hir.nodes[length.0].value_type() != NumericType::Int32 {
                        return Err(super::VerificationError::InvalidValue(result));
                    }
                    let boxed_length =
                        push_value(&mut representations, MachineRepresentation::Tagged);
                    instructions.push(MachineInstruction::plain(
                        MachineOpcode::BoxInt32,
                        vec![
                            MachineOperand::register_input(machine_value(&values, length)),
                            MachineOperand::fixed_register_output(
                                boxed_length,
                                target_spec.integer_argument(2).expect("target argument 2"),
                            ),
                        ],
                    ));
                    let undefined_this =
                        push_value(&mut representations, MachineRepresentation::Tagged);
                    instructions.push(MachineInstruction::plain(
                        MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                        vec![MachineOperand::fixed_register_output(
                            undefined_this,
                            target_spec.integer_argument(3).expect("target argument 3"),
                        )],
                    ));
                    let undefined_new_target =
                        push_value(&mut representations, MachineRepresentation::Tagged);
                    instructions.push(MachineInstruction::plain(
                        MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                        vec![MachineOperand::fixed_register_output(
                            undefined_new_target,
                            target_spec.integer_argument(4).expect("target argument 4"),
                        )],
                    ));
                    let descriptor_index = intern_call_descriptor(
                        &mut call_descriptors,
                        array_construct_call_descriptor(target_spec),
                    );
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        vec![
                            MachineOperand::fixed_register_input(
                                boxed_length,
                                target_spec.integer_argument(2).expect("target argument 2"),
                            ),
                            MachineOperand::fixed_register_input(
                                undefined_this,
                                target_spec.integer_argument(3).expect("target argument 3"),
                            ),
                            MachineOperand::fixed_register_input(
                                undefined_new_target,
                                target_spec.integer_argument(4).expect("target argument 4"),
                            ),
                            MachineOperand::fixed_register_output(
                                result,
                                target_spec.integer_result(),
                            ),
                        ],
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call.safepoint = Some(super::SafepointId(next_safepoint));
                    next_safepoint = next_safepoint
                        .checked_add(1)
                        .expect("bounded scalar function safepoint count");
                    call
                }
                NumericNode::PropertyLoad { .. } | NumericNode::PropertyStore { .. } => {
                    unreachable!("property probe is selected before ordinary nodes")
                }
                NumericNode::IntegerConstant(value) => MachineInstruction::plain(
                    MachineOpcode::IntegerConstant(i64::from(value)),
                    vec![MachineOperand::register_output(result)],
                ),
                NumericNode::BooleanConstant(value) => MachineInstruction::plain(
                    MachineOpcode::IntegerConstant(i64::from(value)),
                    vec![MachineOperand::register_output(result)],
                ),
                NumericNode::Constant(value) => MachineInstruction::plain(
                    MachineOpcode::FloatConstant(value.to_bits()),
                    vec![MachineOperand::register_output(result)],
                ),
                NumericNode::WidenInt32(source) => MachineInstruction::plain(
                    MachineOpcode::Int32ToFloat64,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::WidenUint32(source) => MachineInstruction::plain(
                    MachineOpcode::Uint32ToFloat64,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::BooleanToInt32(source) => MachineInstruction::plain(
                    MachineOpcode::BooleanToInt32,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::FloatToInt32(_) => {
                    unreachable!("Float64 ToInt32 selected as a typed leaf call")
                }
                NumericNode::IntegerAdd(left, right) => MachineInstruction::plain(
                    MachineOpcode::IntegerAdd,
                    vec![
                        MachineOperand::register_input(machine_value(&values, left)),
                        MachineOperand::register_input(machine_value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerSub(left, right) => MachineInstruction::plain(
                    MachineOpcode::IntegerSub,
                    vec![
                        MachineOperand::register_input(machine_value(&values, left)),
                        MachineOperand::register_input(machine_value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerMul(left, right) => MachineInstruction::plain(
                    MachineOpcode::IntegerMul,
                    vec![
                        MachineOperand::register_input(machine_value(&values, left)),
                        MachineOperand::register_input(machine_value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerNeg(source) => MachineInstruction::plain(
                    MachineOpcode::IntegerNeg,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerAnd(left, right)
                | NumericNode::IntegerOr(left, right)
                | NumericNode::IntegerXor(left, right)
                | NumericNode::IntegerShiftLeft(left, right)
                | NumericNode::IntegerShiftRight(left, right) => {
                    let opcode = match node {
                        NumericNode::IntegerAnd(..) => MachineOpcode::IntegerAnd,
                        NumericNode::IntegerOr(..) => MachineOpcode::IntegerOr,
                        NumericNode::IntegerXor(..) => MachineOpcode::IntegerXor,
                        NumericNode::IntegerShiftLeft(..) => MachineOpcode::IntegerShiftLeft,
                        NumericNode::IntegerShiftRight(..) => MachineOpcode::IntegerShiftRight,
                        _ => unreachable!("matched binary int32 node"),
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![
                            MachineOperand::register_input(machine_value(&values, left)),
                            MachineOperand::register_input(machine_value(&values, right)),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
                NumericNode::IntegerShiftRightLogical(left, right) => MachineInstruction::plain(
                    MachineOpcode::IntegerShiftRightLogical,
                    vec![
                        MachineOperand::register_input(machine_value(&values, left)),
                        MachineOperand::register_input(machine_value(&values, right)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerNot(source) => MachineInstruction::plain(
                    MachineOpcode::IntegerNot,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerEqual(left, right)
                | NumericNode::IntegerNotEqual(left, right)
                | NumericNode::IntegerLessThan(left, right)
                | NumericNode::IntegerLessEqual(left, right)
                | NumericNode::IntegerGreaterThan(left, right)
                | NumericNode::IntegerGreaterEqual(left, right) => {
                    let opcode = match node {
                        NumericNode::IntegerEqual(..) => MachineOpcode::IntegerEqual,
                        NumericNode::IntegerNotEqual(..) => MachineOpcode::IntegerNotEqual,
                        NumericNode::IntegerLessThan(..) => MachineOpcode::IntegerLessThan,
                        NumericNode::IntegerLessEqual(..) => MachineOpcode::IntegerLessEqual,
                        NumericNode::IntegerGreaterThan(..) => MachineOpcode::IntegerGreaterThan,
                        NumericNode::IntegerGreaterEqual(..) => MachineOpcode::IntegerGreaterEqual,
                        _ => unreachable!("matched int32 comparison node"),
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![
                            MachineOperand::register_input(machine_value(&values, left)),
                            MachineOperand::register_input(machine_value(&values, right)),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
                NumericNode::IntegerAddImmediate(source, immediate)
                | NumericNode::IntegerSubImmediate(source, immediate)
                | NumericNode::IntegerAndImmediate(source, immediate)
                | NumericNode::IntegerLessThanImmediate(source, immediate)
                | NumericNode::IntegerEqualImmediate(source, immediate)
                | NumericNode::IntegerNotEqualImmediate(source, immediate) => {
                    let opcode = match node {
                        NumericNode::IntegerAddImmediate(..) => {
                            MachineOpcode::IntegerAddImmediate(immediate)
                        }
                        NumericNode::IntegerSubImmediate(..) => {
                            MachineOpcode::IntegerSubImmediate(immediate)
                        }
                        NumericNode::IntegerAndImmediate(..) => {
                            MachineOpcode::IntegerAndImmediate(immediate)
                        }
                        NumericNode::IntegerLessThanImmediate(..) => {
                            MachineOpcode::IntegerLessThanImmediate(immediate)
                        }
                        NumericNode::IntegerEqualImmediate(..) => {
                            MachineOpcode::IntegerEqualImmediate(immediate)
                        }
                        NumericNode::IntegerNotEqualImmediate(..) => {
                            MachineOpcode::IntegerNotEqualImmediate(immediate)
                        }
                        _ => unreachable!("matched immediate integer node"),
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![
                            MachineOperand::register_input(machine_value(&values, source)),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
                NumericNode::Add(left, right)
                | NumericNode::Sub(left, right)
                | NumericNode::Mul(left, right)
                | NumericNode::Div(left, right) => {
                    let opcode = match node {
                        NumericNode::Add(..) => MachineOpcode::FloatAdd,
                        NumericNode::Sub(..) => MachineOpcode::FloatSub,
                        NumericNode::Mul(..) => MachineOpcode::FloatMul,
                        NumericNode::Div(..) => MachineOpcode::FloatDiv,
                        _ => unreachable!("matched binary numeric node"),
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![
                            MachineOperand::register_input(machine_value(&values, left)),
                            MachineOperand::register_input(machine_value(&values, right)),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
                NumericNode::Neg(source) => MachineInstruction::plain(
                    MachineOpcode::FloatNeg,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::IntegerToBoolean(source) => MachineInstruction::plain(
                    MachineOpcode::IntegerToBoolean,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::TaggedToBoolean(source) => {
                    let padding = push_value(&mut representations, MachineRepresentation::Tagged);
                    instructions.push(MachineInstruction::plain(
                        MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                        vec![MachineOperand::register_output(padding)],
                    ));
                    let descriptor_index = intern_leaf_boolean_call_descriptor(
                        target_spec,
                        &mut call_descriptors,
                        otter_vm::native_abi::STUB_TO_BOOLEAN_LEAF,
                        2,
                    );
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        vec![
                            MachineOperand::fixed_register_input(
                                machine_value(&values, source),
                                target_spec.integer_argument(1).expect("target argument 1"),
                            ),
                            MachineOperand::fixed_register_input(
                                padding,
                                target_spec.integer_argument(2).expect("target argument 2"),
                            ),
                            MachineOperand::fixed_register_output(
                                result,
                                target_spec.integer_result(),
                            ),
                        ],
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call
                }
                NumericNode::TaggedNullishEqual {
                    value,
                    equal,
                    byte_pc,
                } => {
                    let mut compare = MachineInstruction::plain(
                        MachineOpcode::TaggedNullishEqual { byte_pc, equal },
                        vec![
                            MachineOperand::register_input(machine_value(&values, value)),
                            MachineOperand::register_output(result),
                        ],
                    );
                    compare.clobbers = tagged_nullish_equal_clobbers(target_spec);
                    compare
                }
                NumericNode::TaggedStrictEqual(left, right) => {
                    let left = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        left,
                    );
                    let right = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        right,
                    );
                    let descriptor_index = intern_leaf_boolean_call_descriptor(
                        target_spec,
                        &mut call_descriptors,
                        otter_vm::native_abi::STUB_STRICT_EQ_LEAF,
                        2,
                    );
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        vec![
                            MachineOperand::fixed_register_input(
                                left,
                                target_spec.integer_argument(1).expect("target argument 1"),
                            ),
                            MachineOperand::fixed_register_input(
                                right,
                                target_spec.integer_argument(2).expect("target argument 2"),
                            ),
                            MachineOperand::fixed_register_output(
                                result,
                                target_spec.integer_result(),
                            ),
                        ],
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call
                }
                NumericNode::TaggedStringConcat(left, right) => {
                    let left = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        left,
                    );
                    let right = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        right,
                    );
                    let padding = push_value(&mut representations, MachineRepresentation::Tagged);
                    instructions.push(MachineInstruction::plain(
                        MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                        vec![MachineOperand::register_output(padding)],
                    ));
                    let descriptor_index = intern_call_descriptor(
                        &mut call_descriptors,
                        string_concat_call_descriptor(target_spec),
                    );
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        vec![
                            MachineOperand::fixed_register_input(
                                left,
                                target_spec.integer_argument(2).expect("target argument 2"),
                            ),
                            MachineOperand::fixed_register_input(
                                right,
                                target_spec.integer_argument(3).expect("target argument 3"),
                            ),
                            MachineOperand::fixed_register_input(
                                padding,
                                target_spec.integer_argument(4).expect("target argument 4"),
                            ),
                            MachineOperand::fixed_register_output(
                                result,
                                target_spec.integer_result(),
                            ),
                        ],
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call.safepoint = Some(super::SafepointId(next_safepoint));
                    next_safepoint = next_safepoint
                        .checked_add(1)
                        .expect("bounded scalar function safepoint count");
                    call
                }
                NumericNode::NativeLeaf {
                    source,
                    target,
                    value_type,
                    argument_start,
                    byte_pc,
                } => {
                    let descriptor = super::native_leaf::descriptor(
                        target_spec,
                        target,
                        byte_pc,
                        if value_type == NumericType::Int32 {
                            MachineRepresentation::Int32
                        } else {
                            MachineRepresentation::Tagged
                        },
                    )
                    .ok_or(super::VerificationError::InvalidValue(result))?;
                    let descriptor_index =
                        intern_call_descriptor(&mut call_descriptors, descriptor);
                    let source = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        source,
                    );
                    let mut operands = vec![MachineOperand::fixed_register_input(
                        source,
                        target_spec.callee_register(),
                    )];
                    let start = argument_start as usize;
                    let end = start
                        .checked_add(usize::from(target.argument_count))
                        .ok_or(super::VerificationError::InvalidValue(result))?;
                    for (index, &argument) in hir
                        .operand_values
                        .get(start..end)
                        .ok_or(super::VerificationError::InvalidValue(result))?
                        .iter()
                        .enumerate()
                    {
                        let argument = if value_type == NumericType::Int32 {
                            machine_value(&values, argument)
                        } else {
                            tagged_call_argument(
                                hir,
                                &values,
                                &mut representations,
                                &mut instructions,
                                argument,
                            )
                        };
                        operands.push(MachineOperand::fixed_register_input(
                            argument,
                            target_spec
                                .integer_argument(index + 1)
                                .expect("target static-native argument"),
                        ));
                    }
                    operands.push(MachineOperand::fixed_register_output(
                        result,
                        target_spec.integer_result(),
                    ));
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        operands,
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call
                }
                NumericNode::DirectCall {
                    source,
                    target,
                    arguments,
                    logical_pc,
                    byte_pc,
                    exceptional_edge,
                } => {
                    let source_value = tagged_call_argument(
                        hir,
                        &values,
                        &mut representations,
                        &mut instructions,
                        source,
                    );
                    let (argument_mode, arguments) = match arguments {
                        NumericDirectCallArguments::Fixed { start, count } => {
                            let argument_start = usize::try_from(start)
                                .map_err(|_| super::VerificationError::InvalidValue(result))?;
                            let argument_count = usize::try_from(count)
                                .map_err(|_| super::VerificationError::InvalidValue(result))?;
                            let argument_end = argument_start
                                .checked_add(argument_count)
                                .ok_or(super::VerificationError::InvalidValue(result))?;
                            let arguments = hir
                                .operand_values
                                .get(argument_start..argument_end)
                                .ok_or(super::VerificationError::InvalidValue(result))?
                                .iter()
                                .copied()
                                .map(|argument| {
                                    tagged_call_argument(
                                        hir,
                                        &values,
                                        &mut representations,
                                        &mut instructions,
                                        argument,
                                    )
                                })
                                .collect::<Vec<_>>();
                            (DirectCallArgumentMode::Fixed, arguments)
                        }
                        NumericDirectCallArguments::Spread(argument) => (
                            DirectCallArgumentMode::Spread,
                            vec![tagged_call_argument(
                                hir,
                                &values,
                                &mut representations,
                                &mut instructions,
                                argument,
                            )],
                        ),
                    };
                    let target = hir
                        .direct_call_targets
                        .get(usize::from(target))
                        .ok_or(super::VerificationError::InvalidValue(result))?;
                    let construct_receiver = matches!(
                        &target.kind,
                        NumericDirectCallKind::Construct | NumericDirectCallKind::SuperConstruct
                    )
                    .then(|| {
                        let receiver =
                            push_value(&mut representations, MachineRepresentation::Tagged);
                        instructions.push(MachineInstruction::plain(
                            MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                            vec![MachineOperand::register_output(receiver)],
                        ));
                        receiver
                    });
                    let descriptor = direct_call_descriptor(
                        target_spec,
                        target,
                        hir.frame_states
                            .iter()
                            .find(|state| state.point == NumericFramePoint::Node(node_value))
                            .and_then(|state| state.frames.last())
                            .filter(|frame| frame.byte_pc == byte_pc)
                            .ok_or(super::VerificationError::InvalidValue(result))?
                            .function_id,
                        logical_pc,
                        byte_pc,
                        argument_mode,
                        arguments.len(),
                        exceptional_edge.map(|edge| {
                            let edge = usize::from(edge);
                            selection_cfg
                                .split_edges
                                .get(&(block_index, edge))
                                .copied()
                                .unwrap_or_else(|| selection_cfg.originals[block.successors[edge]])
                        }),
                    )
                    .ok_or(super::VerificationError::InvalidValue(result))?;
                    let descriptor_index =
                        intern_call_descriptor(&mut call_descriptors, descriptor);
                    let mut operands = Vec::with_capacity(arguments.len() + 2);
                    operands.push(MachineOperand::register_input(source_value));
                    operands.extend(arguments.into_iter().map(MachineOperand::register_input));
                    operands.push(MachineOperand::register_output(result));
                    // A freshly allocated construct receiver is not part of
                    // the pre-call interpreter state. Keep that one explicit
                    // runtime root; ordinary arguments and frame values are
                    // derived after the complete CFG is known.
                    if let Some(receiver) = construct_receiver {
                        operands.push(MachineOperand::runtime_root(receiver));
                    }
                    let mut call = MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        operands,
                    );
                    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
                    call.safepoint = Some(super::SafepointId(next_safepoint));
                    next_safepoint = next_safepoint
                        .checked_add(1)
                        .expect("bounded scalar function safepoint count");
                    call
                }
                NumericNode::ColdCallExit {
                    kind,
                    logical_pc,
                    byte_pc,
                    exceptional_edge,
                } => {
                    let landing_pad = exceptional_edge.map(|edge| {
                        let edge = usize::from(edge);
                        selection_cfg
                            .split_edges
                            .get(&(block_index, edge))
                            .copied()
                            .unwrap_or_else(|| selection_cfg.originals[block.successors[edge]])
                    });
                    let descriptor_index = intern_call_descriptor(
                        &mut call_descriptors,
                        cold_call_exit_descriptor(
                            kind,
                            hir.function_id,
                            logical_pc,
                            byte_pc,
                            landing_pad,
                        ),
                    );
                    MachineInstruction::plain(
                        MachineOpcode::Call(descriptor_index as u32),
                        vec![MachineOperand::register_output(result)],
                    )
                }
                NumericNode::FloatToBoolean(source) => MachineInstruction::plain(
                    MachineOpcode::FloatToBoolean,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::BooleanNot(source) => MachineInstruction::plain(
                    MachineOpcode::BooleanNot,
                    vec![
                        MachineOperand::register_input(machine_value(&values, source)),
                        MachineOperand::register_output(result),
                    ],
                ),
                NumericNode::Equal(left, right)
                | NumericNode::NotEqual(left, right)
                | NumericNode::LessThan(left, right)
                | NumericNode::LessEqual(left, right)
                | NumericNode::GreaterThan(left, right)
                | NumericNode::GreaterEqual(left, right) => {
                    let opcode = match node {
                        NumericNode::Equal(..) => MachineOpcode::FloatEqual,
                        NumericNode::NotEqual(..) => MachineOpcode::FloatNotEqual,
                        NumericNode::LessThan(..) => MachineOpcode::FloatLessThan,
                        NumericNode::LessEqual(..) => MachineOpcode::FloatLessEqual,
                        NumericNode::GreaterThan(..) => MachineOpcode::FloatGreaterThan,
                        NumericNode::GreaterEqual(..) => MachineOpcode::FloatGreaterEqual,
                        _ => unreachable!("matched Float64 comparison node"),
                    };
                    MachineInstruction::plain(
                        opcode,
                        vec![
                            MachineOperand::register_input(machine_value(&values, left)),
                            MachineOperand::register_input(machine_value(&values, right)),
                            MachineOperand::register_output(result),
                        ],
                    )
                }
                NumericNode::Rem(..) | NumericNode::Pow(..) => {
                    unreachable!("numeric leaf calls are selected before ordinary nodes")
                }
            };
            let frame_point = NumericFramePoint::Node(node_value);
            if let Some(&state_index) = frame_state_indices.get(&frame_point) {
                match node.frame_state_purpose() {
                    Some(NumericFrameStatePurpose::TaggedRoots) => {
                        attach_frame_state_tagged_roots(
                            hir,
                            &values,
                            state_index,
                            &mut instruction,
                        );
                    }
                    Some(NumericFrameStatePurpose::ExactDeopt) => attach_frame_state(
                        hir,
                        &values,
                        state_index,
                        exit_specs[&frame_point].clone(),
                        &mut instruction,
                    ),
                    None => {
                        return Err(super::VerificationError::OpcodeSignatureMismatch(
                            MachineInstructionId(instructions.len() as u32),
                        ));
                    }
                }
            }
            if matches!(node, NumericNode::DirectCall { .. }) {
                let state_index = *frame_state_indices
                    .get(&frame_point)
                    .ok_or(super::VerificationError::InvalidValue(result))?;
                inline_reentry::select_frames(
                    hir,
                    state_index,
                    &values,
                    &mut representations,
                    &mut instructions,
                    &mut instruction,
                );
            }
            instructions.push(instruction);
        }

        if selected_binding_guard {
            continue;
        }

        let mut terminator = match block.terminator {
            NumericTerminator::Jump => MachineInstruction::plain(MachineOpcode::Jump, Vec::new()),
            NumericTerminator::Branch {
                condition,
                when_true,
            } => MachineInstruction::plain(
                MachineOpcode::BranchIf(when_true),
                vec![MachineOperand::register_input(machine_value(
                    &values, condition,
                ))],
            ),
            NumericTerminator::Return(value) | NumericTerminator::Throw(value) => {
                let exit_opcode = if matches!(block.terminator, NumericTerminator::Throw(_)) {
                    MachineOpcode::Throw
                } else {
                    MachineOpcode::Return
                };
                if hir.nodes[value.0].value_type() == NumericType::Tagged {
                    let mut ret = MachineInstruction::plain(
                        exit_opcode,
                        vec![MachineOperand::register_input(machine_value(
                            &values, value,
                        ))],
                    );
                    ret.control = ControlFlow::Return;
                    instructions.push(ret);
                    let end = MachineInstructionId(instructions.len() as u32);
                    blocks.push(machine_block(
                        hir,
                        &selection_cfg,
                        block_index,
                        &values,
                        first,
                        end,
                    ));
                    continue;
                }
                let boxed = push_value(&mut representations, MachineRepresentation::Tagged);
                let box_opcode = match hir.nodes[value.0].value_type() {
                    NumericType::Tagged => unreachable!("tagged returns bypass boxing"),
                    NumericType::Int32 => MachineOpcode::BoxInt32,
                    NumericType::Uint32 => MachineOpcode::BoxUint32,
                    NumericType::Number => MachineOpcode::BoxNumber,
                    NumericType::Boolean => MachineOpcode::BoxBoolean,
                };
                instructions.push(MachineInstruction::plain(
                    box_opcode,
                    vec![
                        MachineOperand::register_input(machine_value(&values, value)),
                        MachineOperand::register_output(boxed),
                    ],
                ));
                let mut ret = MachineInstruction::plain(
                    exit_opcode,
                    vec![MachineOperand::register_input(boxed)],
                );
                ret.control = ControlFlow::Return;
                instructions.push(ret);
                let end = MachineInstructionId(instructions.len() as u32);
                blocks.push(machine_block(
                    hir,
                    &selection_cfg,
                    block_index,
                    &values,
                    first,
                    end,
                ));
                continue;
            }
        };
        terminator.control = ControlFlow::Branch;
        instructions.push(terminator);
        let end = MachineInstructionId(instructions.len() as u32);
        blocks.push(machine_block(
            hir,
            &selection_cfg,
            block_index,
            &values,
            first,
            end,
        ));
    }

    super::committed_probe::expand(
        target_spec,
        &mut representations,
        &mut call_descriptors,
        &committed_probes,
        &mut blocks,
        &mut instructions,
    )?;
    super::truthiness::expand(
        &mut representations,
        &call_descriptors,
        &mut blocks,
        &mut instructions,
    );
    InstructionSequence::new_selected_with_packed_double_view_caches(
        target_spec,
        selection_cfg.originals[0],
        representations,
        call_descriptors,
        machine_frame_states(hir),
        blocks,
        instructions,
        u8::try_from(packed_double_view_caches.caches.len())
            .map_err(|_| super::VerificationError::InvalidEntry)?,
    )
}

fn machine_binding_target(target: NumericBindingTarget) -> MachineBindingTarget {
    match target {
        NumericBindingTarget::GlobalThis => MachineBindingTarget::GlobalThis,
        NumericBindingTarget::Upvalue { index } => MachineBindingTarget::Upvalue { index },
        NumericBindingTarget::Global(proof) => MachineBindingTarget::Global(proof),
    }
}

fn select_binding_hit_block(
    target_spec: &TargetSpec,
    hir: &NumericFunction,
    cfg: &SelectionCfg,
    block_index: usize,
    values: BindingSelectedValues,
    inputs: [Option<MachineValue>; 2],
    instructions: &mut Vec<MachineInstruction>,
) -> Result<MachineBlockData, super::VerificationError> {
    let first = MachineInstructionId(instructions.len() as u32);
    let (node_value, target, _) = binding_site(hir, block_index).ok_or(
        super::VerificationError::InvalidBlock(cfg.originals[block_index]),
    )?;
    let NumericNode::Binding {
        semantics, byte_pc, ..
    } = hir.nodes[node_value.0]
    else {
        unreachable!("binding site owns a binding node")
    };
    let target = target
        .map(machine_binding_target)
        .ok_or(super::VerificationError::OpcodeSignatureMismatch(first))?;
    let mut operands = vec![
        MachineOperand::location_input(values.owner),
        MachineOperand::location_input(values.storage),
    ];
    if matches!(
        semantics,
        otter_bytecode::opcode_schema::BindingSemantics::Write(_)
    ) {
        operands.push(MachineOperand::location_input(
            inputs[0].ok_or(super::VerificationError::OpcodeSignatureMismatch(first))?,
        ));
    }
    if semantics.result_operand().is_some() {
        operands.push(MachineOperand::register_output(
            values
                .hit_value
                .ok_or(super::VerificationError::OpcodeSignatureMismatch(first))?,
        ));
    }
    let mut hit = MachineInstruction::plain(
        MachineOpcode::BindingHit {
            byte_pc,
            semantics,
            target,
        },
        operands,
    );
    hit.clobbers = binding_hit_clobbers(target_spec);
    instructions.push(hit);
    if matches!(
        semantics,
        otter_bytecode::opcode_schema::BindingSemantics::Write(_)
    ) {
        let mut barrier = MachineInstruction::plain(
            MachineOpcode::BindingWriteBarrier,
            vec![
                MachineOperand::location_input(values.owner),
                MachineOperand::location_input(
                    inputs[0].ok_or(super::VerificationError::OpcodeSignatureMismatch(first))?,
                ),
            ],
        );
        barrier.clobbers = binding_write_barrier_clobbers(target_spec);
        instructions.push(barrier);
    }
    let mut jump = MachineInstruction::plain(MachineOpcode::Jump, Vec::new());
    jump.control = ControlFlow::Branch;
    instructions.push(jump);
    let end = MachineInstructionId(instructions.len() as u32);
    let selected = cfg.bindings[&block_index];
    Ok(MachineBlockData {
        first,
        end,
        predecessors: vec![cfg.originals[block_index]],
        successors: vec![selected.join],
        parameters: Vec::new(),
        successor_arguments: vec![values.hit_value.into_iter().collect()],
    })
}

#[allow(clippy::too_many_arguments)]
fn select_binding_cold_block(
    target_spec: &TargetSpec,
    hir: &NumericFunction,
    cfg: &SelectionCfg,
    block_index: usize,
    values: BindingSelectedValues,
    inputs: [Option<MachineValue>; 2],
    machine_values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    frame_state_indices: &BTreeMap<NumericFramePoint, usize>,
    call_descriptors: &mut Vec<CallDescriptor>,
    next_safepoint: &mut u32,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<MachineBlockData, super::VerificationError> {
    let first = MachineInstructionId(instructions.len() as u32);
    let (node_value, _, exceptional_edge) = binding_site(hir, block_index).ok_or(
        super::VerificationError::InvalidBlock(cfg.originals[block_index]),
    )?;
    let NumericNode::Binding {
        logical_pc,
        byte_pc,
        ..
    } = hir.nodes[node_value.0]
    else {
        unreachable!("binding site owns a binding node")
    };
    let inputs = inputs.into_iter().flatten().collect::<Vec<_>>();
    let semantic_arity = u8::try_from(inputs.len())
        .map_err(|_| super::VerificationError::OpcodeSignatureMismatch(first))?;
    let descriptor = binding_value_descriptor(target_spec, logical_pc, byte_pc, semantic_arity);
    let descriptor_index = intern_call_descriptor(call_descriptors, descriptor);
    let mut operands = inputs
        .iter()
        .copied()
        .map(MachineOperand::location_input)
        .collect::<Vec<_>>();
    operands.push(MachineOperand::register_output(values.cold_payload));
    operands.push(MachineOperand::register_output(values.status));
    let mut call =
        MachineInstruction::plain(MachineOpcode::Call(descriptor_index as u32), operands);
    call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
    call.safepoint = Some(SafepointId(*next_safepoint));
    *next_safepoint = next_safepoint
        .checked_add(1)
        .expect("bounded scalar function safepoint count");
    let state_index = frame_state_indices[&NumericFramePoint::Node(node_value)];
    inline_reentry::select_frames(
        hir,
        state_index,
        machine_values,
        representations,
        instructions,
        &mut call,
    );
    attach_frame_state_tagged_roots(hir, machine_values, state_index, &mut call);
    instructions.push(call);
    let mut branch = MachineInstruction::plain(
        MachineOpcode::BranchNativeStatus,
        vec![MachineOperand::register_input(values.status)],
    );
    branch.clobbers = target_spec
        .clobbers(TargetClobberSet::StatusScratch)
        .to_vec();
    branch.control = ControlFlow::Branch;
    instructions.push(branch);
    let end = MachineInstructionId(instructions.len() as u32);
    let selected = cfg.bindings[&block_index];
    let throw = exceptional_edge.map_or_else(
        || {
            selected
                .throw
                .ok_or(super::VerificationError::OpcodeSignatureMismatch(first))
        },
        |edge| {
            cfg.split_edges
                .get(&(block_index, edge))
                .copied()
                .ok_or(super::VerificationError::OpcodeSignatureMismatch(first))
        },
    )?;
    Ok(MachineBlockData {
        first,
        end,
        predecessors: vec![cfg.originals[block_index]],
        successors: vec![selected.success, throw, selected.fatal],
        parameters: Vec::new(),
        successor_arguments: vec![Vec::new(), Vec::new(), Vec::new()],
    })
}

fn select_binding_success_block(
    hir: &NumericFunction,
    cfg: &SelectionCfg,
    block_index: usize,
    values: BindingSelectedValues,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<MachineBlockData, super::VerificationError> {
    let first = MachineInstructionId(instructions.len() as u32);
    let (node_value, _, _) = binding_site(hir, block_index).ok_or(
        super::VerificationError::InvalidBlock(cfg.originals[block_index]),
    )?;
    let NumericNode::Binding { semantics, .. } = hir.nodes[node_value.0] else {
        unreachable!("binding site owns a binding node")
    };
    let mut jump = MachineInstruction::plain(MachineOpcode::Jump, Vec::new());
    jump.control = ControlFlow::Branch;
    instructions.push(jump);
    Ok(MachineBlockData {
        first,
        end: MachineInstructionId(instructions.len() as u32),
        predecessors: vec![cfg.bindings[&block_index].cold],
        successors: vec![cfg.bindings[&block_index].join],
        parameters: Vec::new(),
        successor_arguments: vec![
            semantics
                .result_operand()
                .map(|_| vec![values.cold_payload])
                .unwrap_or_default(),
        ],
    })
}

fn select_binding_throw_block(
    cfg: &SelectionCfg,
    block_index: usize,
    values: BindingSelectedValues,
    instructions: &mut Vec<MachineInstruction>,
) -> MachineBlockData {
    let first = MachineInstructionId(instructions.len() as u32);
    let mut throw = MachineInstruction::plain(
        MachineOpcode::Throw,
        vec![MachineOperand::register_input(values.cold_payload)],
    );
    throw.control = ControlFlow::Return;
    instructions.push(throw);
    MachineBlockData {
        first,
        end: MachineInstructionId(instructions.len() as u32),
        predecessors: vec![cfg.bindings[&block_index].cold],
        successors: Vec::new(),
        parameters: Vec::new(),
        successor_arguments: Vec::new(),
    }
}

fn select_binding_fatal_block(
    cfg: &SelectionCfg,
    block_index: usize,
    instructions: &mut Vec<MachineInstruction>,
) -> MachineBlockData {
    let first = MachineInstructionId(instructions.len() as u32);
    let mut fatal = MachineInstruction::plain(MachineOpcode::Fatal, Vec::new());
    fatal.control = ControlFlow::Return;
    instructions.push(fatal);
    MachineBlockData {
        first,
        end: MachineInstructionId(instructions.len() as u32),
        predecessors: vec![cfg.bindings[&block_index].cold],
        successors: Vec::new(),
        parameters: Vec::new(),
        successor_arguments: Vec::new(),
    }
}

fn select_binding_join_block(
    hir: &NumericFunction,
    cfg: &SelectionCfg,
    block_index: usize,
    machine_values: &[MachineValue],
    instructions: &mut Vec<MachineInstruction>,
) -> Result<MachineBlockData, super::VerificationError> {
    let first = MachineInstructionId(instructions.len() as u32);
    let (node_value, target, exceptional_edge) = binding_site(hir, block_index).ok_or(
        super::VerificationError::InvalidBlock(cfg.originals[block_index]),
    )?;
    let NumericNode::Binding {
        semantics, byte_pc, ..
    } = hir.nodes[node_value.0]
    else {
        unreachable!("binding site owns a binding node")
    };
    instructions.push(MachineInstruction::plain(
        MachineOpcode::BindingJoin { byte_pc, semantics },
        Vec::new(),
    ));
    if hir.blocks[block_index].terminator != NumericTerminator::Jump {
        return Err(super::VerificationError::OpcodeSignatureMismatch(first));
    }
    let mut jump = MachineInstruction::plain(MachineOpcode::Jump, Vec::new());
    jump.control = ControlFlow::Branch;
    instructions.push(jump);
    let normal_edges = hir.blocks[block_index]
        .successors
        .iter()
        .enumerate()
        .filter(|(edge, _)| Some(*edge) != exceptional_edge)
        .collect::<Vec<_>>();
    let successors = normal_edges
        .iter()
        .map(|&(edge, &successor)| {
            cfg.split_edges
                .get(&(block_index, edge))
                .copied()
                .unwrap_or(cfg.originals[successor])
        })
        .collect::<Vec<_>>();
    let successor_arguments = normal_edges
        .iter()
        .map(|&(edge, _)| {
            if cfg.split_edges.contains_key(&(block_index, edge)) {
                Vec::new()
            } else {
                hir.blocks[block_index].successor_arguments[edge]
                    .iter()
                    .map(|&value| machine_value(machine_values, value))
                    .collect()
            }
        })
        .collect::<Vec<_>>();
    let selected = cfg.bindings[&block_index];
    let mut predecessors = Vec::with_capacity(2);
    if target.is_some() {
        predecessors.push(
            selected
                .hit
                .expect("generated binding target has hit block"),
        );
    }
    predecessors.push(selected.success);
    Ok(MachineBlockData {
        first,
        end: MachineInstructionId(instructions.len() as u32),
        predecessors,
        successors,
        parameters: semantics
            .result_operand()
            .map(|_| vec![machine_values[node_value.0]])
            .unwrap_or_default(),
        successor_arguments,
    })
}

#[cfg(test)]
fn select(hir: &NumericFunction) -> Result<InstructionSequence, super::VerificationError> {
    select_with_packed_double_view_caches(
        &TargetSpec::aarch64(),
        hir,
        &NumericPackedDoubleViewCachePlan::default(),
    )
}

fn intern_leaf_boolean_call_descriptor(
    target_spec: &TargetSpec,
    descriptors: &mut Vec<CallDescriptor>,
    target: otter_vm::native_abi::RuntimeStubDescriptor,
    argument_count: usize,
) -> usize {
    intern_call_descriptor(
        descriptors,
        leaf_boolean_call_descriptor(target_spec, target, argument_count),
    )
}

fn element_clobbers(target_spec: &TargetSpec) -> Vec<PhysicalRegister> {
    target_spec.clobbers(TargetClobberSet::Element).to_vec()
}

/// Select a site's own immutable program only when its frame recipe names
/// the same source operation. This also holds after future callee remapping.
fn owned_property_site(
    hir: &NumericFunction,
    node: hir::NumericValue,
    byte_pc: u32,
) -> Option<&super::MachineCacheIrSite> {
    let site = hir.property_sites.get(&node)?;
    let frame = hir
        .frame_states
        .iter()
        .find(|state| state.point == NumericFramePoint::Node(node))?
        .frames
        .last()?;
    (site.byte_pc == byte_pc && frame.byte_pc == byte_pc && frame.function_id == site.function_id)
        .then_some(site)
}

fn property_load_clobbers(target_spec: &TargetSpec) -> Vec<PhysicalRegister> {
    target_spec
        .clobbers(TargetClobberSet::PropertyLoad)
        .to_vec()
}

fn property_store_clobbers(
    target_spec: &TargetSpec,
    _value_is_non_cell: bool,
) -> Vec<PhysicalRegister> {
    target_spec
        .clobbers(TargetClobberSet::PropertyStore)
        .to_vec()
}

const fn property_store_value_is_non_cell(value_type: NumericType) -> bool {
    matches!(
        value_type,
        NumericType::Int32 | NumericType::Uint32 | NumericType::Number | NumericType::Boolean
    )
}

#[allow(clippy::too_many_arguments)]
fn select_cache_ir_property_programs(
    target_spec: &TargetSpec,
    source: &super::MachineCacheIrSite,
    receiver: MachineValue,
    stored: Option<MachineValue>,
    value_is_non_cell: bool,
    exotic_length: bool,
    outputs: property_cfg::Values,
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<(), super::VerificationError> {
    instructions.push(MachineInstruction::plain(
        MachineOpcode::PropertySource {
            function_id: source.function_id,
            logical_pc: source.logical_pc,
            byte_pc: source.byte_pc,
            store: stored.is_some(),
        },
        vec![MachineOperand::register_output(outputs.cell)],
    ));

    let false_value = push_value(representations, MachineRepresentation::Boolean);
    instructions.push(MachineInstruction::plain(
        MachineOpcode::BooleanConstant(false),
        vec![MachineOperand::register_output(false_value)],
    ));
    let undefined = push_value(representations, MachineRepresentation::Tagged);
    instructions.push(MachineInstruction::plain(
        MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
        vec![MachineOperand::register_output(undefined)],
    ));
    let mut accumulated_hit = false_value;
    let mut accumulated_payload = undefined;

    if stored.is_none() && exotic_length {
        let payload = push_value(representations, MachineRepresentation::Tagged);
        let hit = push_value(representations, MachineRepresentation::Boolean);
        let mut operation = MachineInstruction::plain(
            MachineOpcode::ExoticLength {
                byte_pc: source.byte_pc,
            },
            vec![
                MachineOperand::location_input(receiver),
                MachineOperand::register_output(payload),
                MachineOperand::register_output(hit),
            ],
        );
        operation.clobbers = property_load_clobbers(target_spec);
        instructions.push(operation);
        accumulated_payload = append_tagged_select(
            hit,
            payload,
            accumulated_payload,
            representations,
            instructions,
        );
        accumulated_hit = append_boolean_or(accumulated_hit, hit, representations, instructions);
    }

    for program in source.program.iter() {
        let active = push_value(representations, MachineRepresentation::Boolean);
        instructions.push(MachineInstruction::plain(
            MachineOpcode::BooleanConstant(true),
            vec![MachineOperand::register_output(active)],
        ));
        let mut active = active;
        let mut objects = [Some(receiver), None];
        let mut terminal = false;
        let mut committed_store = None;
        for op in program.ops.iter() {
            match *op {
                otter_vm::JitCacheIrOp::GuardShape { object, shape } => {
                    let object = cache_ir_object(&objects, object, instructions.len())?;
                    let next = push_value(representations, MachineRepresentation::Boolean);
                    let mut guard = MachineInstruction::plain(
                        MachineOpcode::CacheIrGuardShape {
                            byte_pc: source.byte_pc,
                            shape,
                        },
                        vec![
                            MachineOperand::location_input(object),
                            MachineOperand::register_input(active),
                            MachineOperand::register_output(next),
                        ],
                    );
                    guard.clobbers = property_load_clobbers(target_spec);
                    instructions.push(guard);
                    active = next;
                }
                otter_vm::JitCacheIrOp::GuardAtomSlot {
                    object,
                    atom,
                    value_byte,
                    writable,
                } => {
                    let object = cache_ir_object(&objects, object, instructions.len())?;
                    let next = push_value(representations, MachineRepresentation::Boolean);
                    let mut guard = MachineInstruction::plain(
                        MachineOpcode::CacheIrGuardAtomSlot {
                            byte_pc: source.byte_pc,
                            atom,
                            value_byte,
                            writable,
                        },
                        vec![
                            MachineOperand::location_input(object),
                            MachineOperand::register_input(active),
                            MachineOperand::register_output(next),
                        ],
                    );
                    guard.clobbers = property_load_clobbers(target_spec);
                    instructions.push(guard);
                    active = next;
                }
                otter_vm::JitCacheIrOp::LoadPrototype { object, result } => {
                    let object = cache_ir_object(&objects, object, instructions.len())?;
                    let prototype = push_value(representations, MachineRepresentation::Tagged);
                    let next = push_value(representations, MachineRepresentation::Boolean);
                    let mut load = MachineInstruction::plain(
                        MachineOpcode::CacheIrLoadPrototype {
                            byte_pc: source.byte_pc,
                        },
                        vec![
                            MachineOperand::location_input(object),
                            MachineOperand::register_input(active),
                            MachineOperand::register_output(prototype),
                            MachineOperand::register_output(next),
                        ],
                    );
                    load.clobbers = property_load_clobbers(target_spec);
                    instructions.push(load);
                    let destination = objects.get_mut(result as usize).ok_or(
                        super::VerificationError::OpcodeSignatureMismatch(MachineInstructionId(
                            instructions.len() as u32,
                        )),
                    )?;
                    *destination = Some(prototype);
                    active = next;
                }
                otter_vm::JitCacheIrOp::GuardPrototypeNull { object } => {
                    let object = cache_ir_object(&objects, object, instructions.len())?;
                    let next = push_value(representations, MachineRepresentation::Boolean);
                    let mut guard = MachineInstruction::plain(
                        MachineOpcode::CacheIrGuardPrototypeNull {
                            byte_pc: source.byte_pc,
                        },
                        vec![
                            MachineOperand::location_input(object),
                            MachineOperand::register_input(active),
                            MachineOperand::register_output(next),
                        ],
                    );
                    guard.clobbers = property_load_clobbers(target_spec);
                    instructions.push(guard);
                    active = next;
                }
                otter_vm::JitCacheIrOp::LoadField { object, value_byte } => {
                    if stored.is_some() || terminal {
                        return Err(cache_ir_signature_error(instructions.len()));
                    }
                    let object = cache_ir_object(&objects, object, instructions.len())?;
                    let payload = push_value(representations, MachineRepresentation::Tagged);
                    let hit = push_value(representations, MachineRepresentation::Boolean);
                    let mut load = MachineInstruction::plain(
                        MachineOpcode::CacheIrLoadField {
                            byte_pc: source.byte_pc,
                            value_byte,
                        },
                        vec![
                            MachineOperand::location_input(object),
                            MachineOperand::register_input(active),
                            MachineOperand::register_output(payload),
                            MachineOperand::register_output(hit),
                        ],
                    );
                    load.clobbers = property_load_clobbers(target_spec);
                    instructions.push(load);
                    accumulated_payload = append_tagged_select(
                        hit,
                        payload,
                        accumulated_payload,
                        representations,
                        instructions,
                    );
                    accumulated_hit =
                        append_boolean_or(accumulated_hit, hit, representations, instructions);
                    terminal = true;
                }
                otter_vm::JitCacheIrOp::StoreField { object, value_byte } => {
                    let Some(value) = stored else {
                        return Err(cache_ir_signature_error(instructions.len()));
                    };
                    if terminal {
                        return Err(cache_ir_signature_error(instructions.len()));
                    }
                    let object = cache_ir_object(&objects, object, instructions.len())?;
                    let owner = push_value(representations, MachineRepresentation::Int64);
                    let hit = push_value(representations, MachineRepresentation::Boolean);
                    let mut store = MachineInstruction::plain(
                        MachineOpcode::CacheIrStoreField {
                            byte_pc: source.byte_pc,
                            value_byte,
                        },
                        vec![
                            MachineOperand::location_input(object),
                            MachineOperand::location_input(value),
                            MachineOperand::register_input(active),
                            MachineOperand::register_output(owner),
                            MachineOperand::register_output(hit),
                        ],
                    );
                    store.clobbers = property_store_clobbers(target_spec, value_is_non_cell);
                    instructions.push(store);
                    let mut barrier = MachineInstruction::plain(
                        MachineOpcode::CacheIrWriteBarrier {
                            byte_pc: source.byte_pc,
                            value_is_non_cell,
                        },
                        vec![
                            MachineOperand::location_input(owner),
                            MachineOperand::location_input(value),
                            MachineOperand::register_input(hit),
                        ],
                    );
                    barrier.clobbers = property_store_clobbers(target_spec, value_is_non_cell);
                    instructions.push(barrier);
                    accumulated_hit =
                        append_boolean_or(accumulated_hit, hit, representations, instructions);
                    terminal = true;
                    committed_store = Some((object, owner, hit));
                }
                otter_vm::JitCacheIrOp::GuardExtensible { object, value_byte } => {
                    if terminal {
                        return Err(cache_ir_signature_error(instructions.len()));
                    }
                    let object = cache_ir_object(&objects, object, instructions.len())?;
                    let next = push_value(representations, MachineRepresentation::Boolean);
                    let mut guard = MachineInstruction::plain(
                        MachineOpcode::CacheIrGuardExtensible {
                            byte_pc: source.byte_pc,
                            value_byte,
                        },
                        vec![
                            MachineOperand::location_input(object),
                            MachineOperand::register_input(active),
                            MachineOperand::register_output(next),
                        ],
                    );
                    guard.clobbers = property_load_clobbers(target_spec);
                    instructions.push(guard);
                    active = next;
                }
                otter_vm::JitCacheIrOp::PublishShape {
                    object,
                    shape,
                    new_len,
                    initialize_inline,
                } => {
                    let object = cache_ir_object(&objects, object, instructions.len())?;
                    let Some((stored_object, owner, hit)) = committed_store.take() else {
                        return Err(cache_ir_signature_error(instructions.len()));
                    };
                    if stored_object != object || !terminal {
                        return Err(cache_ir_signature_error(instructions.len()));
                    }
                    let mut publish = MachineInstruction::plain(
                        MachineOpcode::CacheIrPublishShape {
                            byte_pc: source.byte_pc,
                            shape,
                            new_len,
                            initialize_inline,
                        },
                        vec![
                            MachineOperand::location_input(owner),
                            MachineOperand::register_input(hit),
                        ],
                    );
                    publish.clobbers = property_store_clobbers(target_spec, false);
                    instructions.push(publish);
                    let shape_value = push_value(representations, MachineRepresentation::Tagged);
                    instructions.push(MachineInstruction::plain(
                        MachineOpcode::TaggedConstant(u64::from(shape)),
                        vec![MachineOperand::register_output(shape_value)],
                    ));
                    let mut barrier = MachineInstruction::plain(
                        MachineOpcode::CacheIrWriteBarrier {
                            byte_pc: source.byte_pc,
                            value_is_non_cell: false,
                        },
                        vec![
                            MachineOperand::location_input(owner),
                            MachineOperand::location_input(shape_value),
                            MachineOperand::register_input(hit),
                        ],
                    );
                    barrier.clobbers = property_store_clobbers(target_spec, false);
                    instructions.push(barrier);
                }
            }
        }
        if !terminal {
            return Err(cache_ir_signature_error(instructions.len()));
        }
    }

    let join_operands = if stored.is_none() {
        vec![
            MachineOperand::register_input(accumulated_payload),
            MachineOperand::register_input(accumulated_hit),
            MachineOperand::register_output(outputs.payload),
            MachineOperand::register_output(outputs.hit),
        ]
    } else {
        vec![
            MachineOperand::register_input(accumulated_hit),
            MachineOperand::register_output(outputs.hit),
        ]
    };
    instructions.push(MachineInstruction::plain(
        MachineOpcode::CacheIrJoin {
            byte_pc: source.byte_pc,
            store: stored.is_some(),
        },
        join_operands,
    ));
    Ok(())
}

fn cache_ir_signature_error(instruction_count: usize) -> super::VerificationError {
    super::VerificationError::OpcodeSignatureMismatch(MachineInstructionId(
        instruction_count as u32,
    ))
}

fn cache_ir_object(
    objects: &[Option<MachineValue>; 2],
    object: u8,
    instruction_count: usize,
) -> Result<MachineValue, super::VerificationError> {
    objects
        .get(object as usize)
        .copied()
        .flatten()
        .ok_or_else(|| cache_ir_signature_error(instruction_count))
}

fn append_boolean_or(
    left: MachineValue,
    right: MachineValue,
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
) -> MachineValue {
    let result = push_value(representations, MachineRepresentation::Boolean);
    instructions.push(MachineInstruction::plain(
        MachineOpcode::BooleanOr,
        vec![
            MachineOperand::register_input(left),
            MachineOperand::register_input(right),
            MachineOperand::register_output(result),
        ],
    ));
    result
}

fn append_tagged_select(
    condition: MachineValue,
    if_true: MachineValue,
    if_false: MachineValue,
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
) -> MachineValue {
    let result = push_value(representations, MachineRepresentation::Tagged);
    instructions.push(MachineInstruction::plain(
        MachineOpcode::TaggedSelect,
        vec![
            MachineOperand::register_input(condition),
            MachineOperand::register_input(if_true),
            MachineOperand::register_input(if_false),
            MachineOperand::register_output(result),
        ],
    ));
    result
}

fn string_constant_cell_load_clobbers(target_spec: &TargetSpec) -> Vec<PhysicalRegister> {
    target_spec
        .clobbers(TargetClobberSet::StringConstantLoad)
        .to_vec()
}

fn tagged_nullish_equal_clobbers(target_spec: &TargetSpec) -> Vec<PhysicalRegister> {
    target_spec
        .clobbers(TargetClobberSet::StatusScratch)
        .to_vec()
}

fn intern_call_descriptor(
    descriptors: &mut Vec<CallDescriptor>,
    descriptor: CallDescriptor,
) -> usize {
    if let Some(index) = descriptors
        .iter()
        .position(|candidate| *candidate == descriptor)
    {
        return index;
    }
    descriptors.push(descriptor);
    descriptors.len() - 1
}

fn string_concat_call_descriptor(target_spec: &TargetSpec) -> CallDescriptor {
    let mut clobbers = target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec();
    clobbers.retain(|register| *register != target_spec.integer_result());
    CallDescriptor {
        target: CallTarget::RuntimeStub(otter_vm::native_abi::STUB_STRING_CONCAT_ALLOC),
        arguments: vec![MachineRepresentation::Tagged; 3],
        results: vec![MachineRepresentation::Tagged],
        effects: CallEffects::READS_HEAP,
        clobbers,
        exceptional: ExceptionalEdge::None,
        safepoint: SafepointKind::Gc,
    }
}

fn array_construct_call_descriptor(target_spec: &TargetSpec) -> CallDescriptor {
    let mut clobbers = target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec();
    clobbers.retain(|register| *register != target_spec.integer_result());
    CallDescriptor {
        target: CallTarget::RuntimeStub(STUB_ARRAY_CONSTRUCT_ALLOC),
        arguments: vec![MachineRepresentation::Tagged; 3],
        results: vec![MachineRepresentation::Tagged],
        effects: CallEffects::READS_HEAP.union(CallEffects::WRITES_HEAP),
        clobbers,
        exceptional: ExceptionalEdge::None,
        safepoint: SafepointKind::Gc,
    }
}

fn caught_throw_acknowledgement_descriptor(target_spec: &TargetSpec) -> CallDescriptor {
    CallDescriptor {
        target: CallTarget::RuntimeStub(STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW),
        arguments: Vec::new(),
        results: Vec::new(),
        // This leaf mutates VM-owned diagnostic provenance. Describe it as a
        // write so it cannot be treated as a freely movable pure computation.
        effects: CallEffects::WRITES_HEAP,
        clobbers: target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec(),
        exceptional: ExceptionalEdge::None,
        safepoint: SafepointKind::None,
    }
}

fn direct_call_descriptor(
    target_spec: &TargetSpec,
    target: &NumericDirectCallTarget,
    caller_function_id: u32,
    logical_pc: u32,
    byte_pc: u32,
    argument_mode: DirectCallArgumentMode,
    argument_count: usize,
    landing_pad: Option<MachineBlock>,
) -> Option<CallDescriptor> {
    let packet_words = argument_count.checked_add(1)?;
    Some(CallDescriptor {
        target: CallTarget::Direct {
            kind: match target.kind {
                NumericDirectCallKind::Plain => DirectCallKind::Plain,
                NumericDirectCallKind::CallWithThis => DirectCallKind::CallWithThis,
                NumericDirectCallKind::Forward => DirectCallKind::Forward,
                NumericDirectCallKind::Method => DirectCallKind::Method,
                NumericDirectCallKind::Construct => DirectCallKind::Construct,
                NumericDirectCallKind::DerivedConstruct => DirectCallKind::DerivedConstruct,
                NumericDirectCallKind::SuperConstruct => DirectCallKind::SuperConstruct,
                NumericDirectCallKind::DerivedSuperConstruct => {
                    DirectCallKind::DerivedSuperConstruct
                }
            },
            argument_mode,
            candidates: target
                .candidates
                .iter()
                .map(|candidate| DirectCallCandidate {
                    target_index: candidate.target_index,
                    target_count: candidate.target_count,
                    guard: candidate.guard.clone(),
                    callee: candidate.callee,
                })
                .collect(),
            caller_function_id,
            logical_pc,
            byte_pc,
        },
        arguments: vec![MachineRepresentation::Tagged; packet_words],
        results: vec![MachineRepresentation::Tagged],
        effects: CallEffects::READS_HEAP
            .union(CallEffects::WRITES_HEAP)
            .union(CallEffects::INVALIDATES_SHAPES)
            .union(CallEffects::REENTRANT),
        clobbers: target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec(),
        exceptional: landing_pad
            .map(ExceptionalEdge::LandingPad)
            .unwrap_or(ExceptionalEdge::Propagate),
        safepoint: SafepointKind::Gc,
    })
}

fn cold_call_exit_descriptor(
    kind: NumericColdCallKind,
    caller_function_id: u32,
    logical_pc: u32,
    byte_pc: u32,
    landing_pad: Option<MachineBlock>,
) -> CallDescriptor {
    CallDescriptor {
        target: CallTarget::ColdCallExit {
            kind: match kind {
                NumericColdCallKind::Plain => ColdCallKind::Plain,
                NumericColdCallKind::Method => ColdCallKind::Method,
            },
            caller_function_id,
            logical_pc,
            byte_pc,
        },
        arguments: Vec::new(),
        results: vec![MachineRepresentation::Tagged],
        effects: CallEffects::PURE,
        clobbers: Vec::new(),
        exceptional: landing_pad
            .map(ExceptionalEdge::LandingPad)
            .unwrap_or(ExceptionalEdge::Propagate),
        safepoint: SafepointKind::None,
    }
}

fn committed_value_descriptor(
    target_spec: &TargetSpec,
    operation: CommittedValueOperation,
    logical_pc: u32,
    byte_pc: u32,
    semantic_arity: u8,
    landing_pad: Option<MachineBlock>,
) -> CallDescriptor {
    let target = match operation {
        CommittedValueOperation::ObjectProtocol(_) => {
            otter_vm::native_abi::STUB_JIT_OBJECT_PROTOCOL_VALUE
        }
        CommittedValueOperation::Scalar(_) => otter_vm::native_abi::STUB_JIT_SCALAR_VALUE,
    };
    CallDescriptor {
        target: CallTarget::CommittedRuntime {
            target,
            logical_pc,
            byte_pc,
            semantic_arity,
        },
        arguments: vec![MachineRepresentation::Tagged; usize::from(semantic_arity)],
        results: vec![MachineRepresentation::Tagged],
        effects: CallEffects::READS_HEAP
            .union(CallEffects::WRITES_HEAP)
            .union(CallEffects::INVALIDATES_SHAPES)
            .union(CallEffects::REENTRANT),
        clobbers: target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec(),
        exceptional: landing_pad
            .map(ExceptionalEdge::LandingPad)
            .unwrap_or(ExceptionalEdge::Propagate),
        safepoint: SafepointKind::Gc,
    }
}

fn binding_value_descriptor(
    target_spec: &TargetSpec,
    logical_pc: u32,
    byte_pc: u32,
    semantic_arity: u8,
) -> CallDescriptor {
    CallDescriptor {
        target: CallTarget::CommittedRuntime {
            target: otter_vm::native_abi::STUB_JIT_BINDING_VALUE,
            logical_pc,
            byte_pc,
            semantic_arity,
        },
        arguments: vec![MachineRepresentation::Tagged; usize::from(semantic_arity)],
        results: vec![
            MachineRepresentation::Tagged,
            MachineRepresentation::NativeStatus,
        ],
        effects: CallEffects::READS_HEAP
            .union(CallEffects::WRITES_HEAP)
            .union(CallEffects::INVALIDATES_SHAPES)
            .union(CallEffects::REENTRANT),
        clobbers: target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec(),
        exceptional: ExceptionalEdge::None,
        safepoint: SafepointKind::Gc,
    }
}

fn class_super_constructor_descriptor(target_spec: &TargetSpec) -> CallDescriptor {
    CallDescriptor {
        target: CallTarget::RuntimeStub(otter_vm::native_abi::STUB_JIT_CLASS_SUPER_CONSTRUCTOR),
        arguments: vec![MachineRepresentation::Tagged],
        results: vec![MachineRepresentation::Tagged],
        effects: CallEffects::READS_HEAP,
        clobbers: target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec(),
        exceptional: ExceptionalEdge::None,
        safepoint: SafepointKind::None,
    }
}

fn leaf_boolean_call_descriptor(
    target_spec: &TargetSpec,
    target: otter_vm::native_abi::RuntimeStubDescriptor,
    argument_count: usize,
) -> CallDescriptor {
    let mut clobbers = target_spec.clobbers(TargetClobberSet::ScalarCall).to_vec();
    clobbers.retain(|register| *register != target_spec.integer_result());
    CallDescriptor {
        target: CallTarget::RuntimeStub(target),
        arguments: vec![MachineRepresentation::Tagged; argument_count],
        results: vec![MachineRepresentation::Boolean],
        effects: CallEffects::READS_HEAP,
        clobbers,
        exceptional: ExceptionalEdge::None,
        safepoint: SafepointKind::None,
    }
}

fn tagged_call_argument(
    hir: &NumericFunction,
    values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
    source: hir::NumericValue,
) -> MachineValue {
    if hir.nodes[source.0].value_type() == NumericType::Tagged {
        return machine_value(values, source);
    }
    let tagged = push_value(representations, MachineRepresentation::Tagged);
    let opcode = match hir.nodes[source.0].value_type() {
        NumericType::Tagged => unreachable!("tagged call argument returned early"),
        NumericType::Int32 => MachineOpcode::BoxInt32,
        NumericType::Uint32 => MachineOpcode::BoxUint32,
        NumericType::Number => MachineOpcode::BoxNumber,
        NumericType::Boolean => MachineOpcode::BoxBoolean,
    };
    instructions.push(MachineInstruction::plain(
        opcode,
        vec![
            MachineOperand::register_input(machine_value(values, source)),
            MachineOperand::register_output(tagged),
        ],
    ));
    tagged
}

fn attach_frame_state(
    hir: &NumericFunction,
    values: &[MachineValue],
    state_index: usize,
    exits: Box<[MachineExit]>,
    instruction: &mut MachineInstruction,
) {
    let state = &hir.frame_states[state_index];
    let mut values_at_exit = BTreeSet::new();
    for slot in state.frame_slots() {
        let hir::NumericFrameSlot::Value(value) = slot else {
            continue;
        };
        if values_at_exit.insert(*value) {
            instruction
                .operands
                .push(MachineOperand::frame_value(machine_value(values, *value)));
        }
    }
    instruction.frame_state = Some(state_index as u32);
    instruction.exits = exits;
}

fn attach_frame_state_tagged_roots(
    hir: &NumericFunction,
    values: &[MachineValue],
    state_index: usize,
    instruction: &mut MachineInstruction,
) {
    let _ = (hir, values);
    instruction.frame_state = Some(state_index as u32);
}

fn frame_state_exits(
    hir: &NumericFunction,
    state: &hir::NumericFrameState,
    next_exit_id: &mut u32,
) -> Box<[MachineExit]> {
    let specs: &[(ExitReason, ExitAction)] = match state.point {
        NumericFramePoint::Backedge { .. } => &[(ExitReason::Interrupt, ExitAction::Resume)],
        NumericFramePoint::Node(node) => match hir.nodes[node.0] {
            NumericNode::IntegerMul(..) | NumericNode::IntegerNeg(..) => &[
                (ExitReason::Int32Overflow, ExitAction::Recompile),
                (ExitReason::NegativeZero, ExitAction::Recompile),
            ],
            NumericNode::IntegerAdd(..)
            | NumericNode::IntegerSub(..)
            | NumericNode::IntegerAddImmediate(..)
            | NumericNode::IntegerSubImmediate(..) => {
                &[(ExitReason::Int32Overflow, ExitAction::Recompile)]
            }
            NumericNode::InlineConstructGuard { .. }
            | NumericNode::InlineCallGuard { .. }
            | NumericNode::InlineMethodGuard { .. }
            | NumericNode::DirectCall { .. }
            | NumericNode::ColdCallExit { .. }
            | NumericNode::NativeLeaf { .. } => {
                &[(ExitReason::IdentityGuard, ExitAction::Recompile)]
            }
            NumericNode::ConstructReceiver { .. } | NumericNode::ArrayConstruct { .. } => {
                &[(ExitReason::AllocationMiss, ExitAction::Resume)]
            }
            NumericNode::ConstructorFieldStore { .. } => {
                &[(ExitReason::ShapeGuard, ExitAction::Recompile)]
            }
            NumericNode::TaggedToNumber(..)
            | NumericNode::TaggedToInt32(..)
            | NumericNode::ClassSuperConstructor(..)
            | NumericNode::TaggedToBoolean(..)
            | NumericNode::TaggedStrictEqual(..)
            | NumericNode::TaggedNullishEqual { .. }
            | NumericNode::TaggedStringConcat(..) => {
                &[(ExitReason::TypeMismatch, ExitAction::Recompile)]
            }
            _ => &[],
        },
    };
    specs
        .iter()
        .map(|&(reason, action)| {
            let id = DeoptId(*next_exit_id);
            *next_exit_id = next_exit_id.saturating_add(1);
            MachineExit { id, reason, action }
        })
        .collect()
}

fn machine_frame_states(hir: &NumericFunction) -> Vec<super::MachineFrameState> {
    fn slot(slot: &hir::NumericFrameSlot) -> super::MachineFrameSlot {
        match slot {
            hir::NumericFrameSlot::Value(value) => {
                super::MachineFrameSlot::Value(MachineValue(value.0 as u32))
            }
            hir::NumericFrameSlot::Undefined => super::undefined_slot(),
        }
    }
    hir.frame_states
        .iter()
        .enumerate()
        .map(|(index, state)| super::MachineFrameState {
            id: index as u32,
            frames: state
                .frames
                .iter()
                .map(|frame| otter_vm::deopt::DeoptFrame {
                    function_id: frame.function_id,
                    byte_pc: frame.byte_pc,
                    entry: frame
                        .entry
                        .as_ref()
                        .map(|entry| otter_vm::deopt::DeoptFrameEntry {
                            new_target: slot(&entry.new_target),
                            return_register: entry.return_register,
                            this: slot(&entry.this),
                            closure: slot(&entry.closure),
                        }),
                    slots: frame.slots.iter().map(slot).collect(),
                })
                .collect(),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectedBlock {
    Original(usize),
    SplitEdge {
        predecessor: usize,
        edge: usize,
        successor: usize,
    },
    PropertyHit(usize),
    PropertyCold(usize),
    PropertySuccess(usize),
    PropertyThrow(usize),
    PropertyFatal(usize),
    PropertyJoin(usize),
    ElementHit(usize),
    ElementCold(usize),
    ElementSuccess(usize),
    ElementThrow(usize),
    ElementFatal(usize),
    ElementJoin(usize),
    BindingHit(usize),
    BindingCold(usize),
    BindingSuccess(usize),
    BindingThrow(usize),
    BindingFatal(usize),
    BindingJoin(usize),
}

#[derive(Debug, Clone, Copy)]
struct BindingSelectedBlocks {
    hit: Option<MachineBlock>,
    cold: MachineBlock,
    success: MachineBlock,
    throw: Option<MachineBlock>,
    fatal: MachineBlock,
    join: MachineBlock,
}

struct SelectionCfg {
    order: Vec<SelectedBlock>,
    originals: Vec<MachineBlock>,
    split_edges: BTreeMap<(usize, usize), MachineBlock>,
    bindings: BTreeMap<usize, BindingSelectedBlocks>,
    properties: BTreeMap<usize, property_cfg::Blocks>,
    elements: BTreeMap<usize, element_cfg::Blocks>,
}

impl SelectionCfg {
    fn build(
        hir: &NumericFunction,
        packed_double_view_caches: &NumericPackedDoubleViewCachePlan,
    ) -> Self {
        let mut order = Vec::with_capacity(hir.blocks.len());
        let mut originals = vec![MachineBlock(u32::MAX); hir.blocks.len()];
        let mut split_edges = BTreeMap::new();
        let mut bindings = BTreeMap::new();
        let mut properties = BTreeMap::new();
        let mut elements = BTreeMap::new();
        for (successor, original) in originals.iter_mut().enumerate() {
            for (predecessor, edge) in incoming_edges(hir, successor) {
                if is_critical_edge(hir, predecessor, successor)
                    || successor <= predecessor
                    || is_exceptional_hir_edge(hir, predecessor, edge)
                    || edge_requires_representation_conversion(hir, predecessor, edge, successor)
                    || packed_double_view_caches
                        .caches
                        .iter()
                        .any(|cache| cache.entry_edges.contains(&(predecessor, edge)))
                {
                    let block = MachineBlock(order.len() as u32);
                    split_edges.insert((predecessor, edge), block);
                    order.push(SelectedBlock::SplitEdge {
                        predecessor,
                        edge,
                        successor,
                    });
                }
            }
            *original = MachineBlock(order.len() as u32);
            order.push(SelectedBlock::Original(successor));
            if property_cfg::site(hir, successor).is_some() {
                let hit = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::PropertyHit(successor));
                let cold = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::PropertyCold(successor));
                let success = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::PropertySuccess(successor));
                let throw = property_cfg::exceptional(hir, successor)
                    .is_none()
                    .then(|| {
                        let block = MachineBlock(order.len() as u32);
                        order.push(SelectedBlock::PropertyThrow(successor));
                        block
                    });
                let fatal = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::PropertyFatal(successor));
                let join = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::PropertyJoin(successor));
                properties.insert(
                    successor,
                    property_cfg::Blocks {
                        hit,
                        cold,
                        success,
                        throw,
                        fatal,
                        join,
                    },
                );
                continue;
            }
            if element_cfg::site(hir, successor).is_some() {
                let hit = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::ElementHit(successor));
                let cold = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::ElementCold(successor));
                let success = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::ElementSuccess(successor));
                let throw = element_cfg::exceptional(hir, successor).is_none().then(|| {
                    let block = MachineBlock(order.len() as u32);
                    order.push(SelectedBlock::ElementThrow(successor));
                    block
                });
                let fatal = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::ElementFatal(successor));
                let join = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::ElementJoin(successor));
                elements.insert(
                    successor,
                    element_cfg::Blocks {
                        hit,
                        cold,
                        success,
                        throw,
                        fatal,
                        join,
                    },
                );
                continue;
            }
            let Some((_, target, exceptional_edge)) = binding_site(hir, successor) else {
                continue;
            };
            let hit = target.map(|_| {
                let block = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::BindingHit(successor));
                block
            });
            let cold = MachineBlock(order.len() as u32);
            order.push(SelectedBlock::BindingCold(successor));
            let success = MachineBlock(order.len() as u32);
            order.push(SelectedBlock::BindingSuccess(successor));
            let throw = exceptional_edge.is_none().then(|| {
                let block = MachineBlock(order.len() as u32);
                order.push(SelectedBlock::BindingThrow(successor));
                block
            });
            let fatal = MachineBlock(order.len() as u32);
            order.push(SelectedBlock::BindingFatal(successor));
            let join = MachineBlock(order.len() as u32);
            order.push(SelectedBlock::BindingJoin(successor));
            bindings.insert(
                successor,
                BindingSelectedBlocks {
                    hit,
                    cold,
                    success,
                    throw,
                    fatal,
                    join,
                },
            );
        }
        Self {
            order,
            originals,
            split_edges,
            bindings,
            properties,
            elements,
        }
    }

    fn normal_exit(&self, block: usize) -> MachineBlock {
        self.properties
            .get(&block)
            .map(|property| property.join)
            .unwrap_or_else(|| {
                self.elements
                    .get(&block)
                    .map(|element| element.join)
                    .unwrap_or_else(|| {
                        self.bindings
                            .get(&block)
                            .map_or(self.originals[block], |binding| binding.join)
                    })
            })
    }
}

fn binding_site(
    hir: &NumericFunction,
    block: usize,
) -> Option<(
    hir::NumericValue,
    Option<NumericBindingTarget>,
    Option<usize>,
)> {
    let value = *hir.blocks.get(block)?.nodes.last()?;
    let NumericNode::Binding {
        target,
        exceptional_edge,
        ..
    } = hir.nodes.get(value.0)?
    else {
        return None;
    };
    Some((value, *target, exceptional_edge.map(usize::from)))
}

fn is_exceptional_hir_edge(hir: &NumericFunction, predecessor: usize, edge: usize) -> bool {
    exceptional_hir_edge_source(hir, predecessor, edge).is_some()
}

fn exceptional_hir_edge_source(
    hir: &NumericFunction,
    predecessor: usize,
    edge: usize,
) -> Option<NumericValue> {
    hir.blocks[predecessor].nodes.iter().copied().find(|value| {
        let exceptional_edge = match hir.nodes[value.0] {
            NumericNode::PropertyLoad {
                exceptional_edge, ..
            }
            | NumericNode::Binding {
                exceptional_edge, ..
            }
            | NumericNode::CommittedValue {
                exceptional_edge, ..
            }
            | NumericNode::ElementLoad {
                exceptional_edge, ..
            }
            | NumericNode::ElementStore {
                exceptional_edge, ..
            }
            | NumericNode::DirectCall {
                exceptional_edge, ..
            }
            | NumericNode::ColdCallExit {
                exceptional_edge, ..
            } => exceptional_edge,
            _ => None,
        };
        exceptional_edge.is_some_and(|exceptional_edge| usize::from(exceptional_edge) == edge)
    })
}

fn incoming_edges(hir: &NumericFunction, successor: usize) -> Vec<(usize, usize)> {
    hir.blocks
        .iter()
        .enumerate()
        .flat_map(|(predecessor, block)| {
            block
                .successors
                .iter()
                .enumerate()
                .filter_map(move |(edge, &target)| {
                    (target == successor).then_some((predecessor, edge))
                })
        })
        .collect()
}

fn is_critical_edge(hir: &NumericFunction, predecessor: usize, successor: usize) -> bool {
    hir.blocks[predecessor].successors.len() > 1 && hir.blocks[successor].predecessors.len() > 1
}

fn edge_requires_representation_conversion(
    hir: &NumericFunction,
    predecessor: usize,
    edge: usize,
    successor: usize,
) -> bool {
    hir.blocks[predecessor].successor_arguments[edge]
        .iter()
        .zip(&hir.blocks[successor].parameters)
        .any(|(&argument, &parameter)| {
            hir.nodes[argument.0].value_type() != hir.nodes[parameter.0].value_type()
        })
}

fn select_edge_argument(
    hir: &NumericFunction,
    values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
    argument: hir::NumericValue,
    parameter: hir::NumericValue,
    successor: MachineBlock,
) -> Result<MachineValue, super::VerificationError> {
    let source_type = hir.nodes[argument.0].value_type();
    let target_type = hir.nodes[parameter.0].value_type();
    let source = machine_value(values, argument);
    if source_type == target_type {
        return Ok(source);
    }
    let (opcode, representation) = match (source_type, target_type) {
        (NumericType::Int32, NumericType::Number) => (
            MachineOpcode::Int32ToFloat64,
            MachineRepresentation::Float64,
        ),
        (NumericType::Uint32, NumericType::Number) => (
            MachineOpcode::Uint32ToFloat64,
            MachineRepresentation::Float64,
        ),
        (NumericType::Int32, NumericType::Tagged) => {
            (MachineOpcode::BoxInt32, MachineRepresentation::Tagged)
        }
        (NumericType::Uint32, NumericType::Tagged) => {
            (MachineOpcode::BoxUint32, MachineRepresentation::Tagged)
        }
        (NumericType::Number, NumericType::Tagged) => {
            (MachineOpcode::BoxNumber, MachineRepresentation::Tagged)
        }
        (NumericType::Boolean, NumericType::Tagged) => {
            (MachineOpcode::BoxBoolean, MachineRepresentation::Tagged)
        }
        _ => {
            return Err(super::VerificationError::BlockParameterRepresentation(
                successor,
                machine_value(values, parameter),
            ));
        }
    };
    let converted = push_value(representations, representation);
    instructions.push(MachineInstruction::plain(
        opcode,
        vec![
            MachineOperand::register_input(source),
            MachineOperand::register_output(converted),
        ],
    ));
    Ok(converted)
}

fn machine_block(
    hir: &NumericFunction,
    selection_cfg: &SelectionCfg,
    block_index: usize,
    values: &[MachineValue],
    first: MachineInstructionId,
    end: MachineInstructionId,
) -> MachineBlockData {
    let block = &hir.blocks[block_index];
    let mut predecessors = incoming_edges(hir, block_index)
        .into_iter()
        .map(|(predecessor, edge)| {
            selection_cfg
                .split_edges
                .get(&(predecessor, edge))
                .copied()
                .unwrap_or_else(|| selection_cfg.normal_exit(predecessor))
        })
        .collect::<Vec<_>>();
    predecessors.sort_unstable();
    MachineBlockData {
        first,
        end,
        predecessors,
        successors: block
            .successors
            .iter()
            .enumerate()
            .map(|(edge, &successor)| {
                selection_cfg
                    .split_edges
                    .get(&(block_index, edge))
                    .copied()
                    .unwrap_or(selection_cfg.originals[successor])
            })
            .collect(),
        parameters: block
            .parameters
            .iter()
            .map(|&value| machine_value(values, value))
            .collect(),
        successor_arguments: block
            .successor_arguments
            .iter()
            .enumerate()
            .map(|(edge, arguments)| {
                if selection_cfg.split_edges.contains_key(&(block_index, edge)) {
                    Vec::new()
                } else {
                    arguments
                        .iter()
                        .map(|&value| machine_value(values, value))
                        .collect()
                }
            })
            .collect(),
    }
}

fn push_value(
    representations: &mut Vec<MachineRepresentation>,
    representation: MachineRepresentation,
) -> MachineValue {
    let value = MachineValue(representations.len() as u32);
    representations.push(representation);
    value
}

fn machine_value(values: &[MachineValue], value: hir::NumericValue) -> MachineValue {
    values[value.0]
}

#[cfg(test)]
mod tests {
    #[test]
    fn inline_frame_only_values_survive_selection_and_allocation() {
        use hir::{NumericBlock, NumericFrameSlot, NumericFrameState, NumericValue};
        use otter_vm::deopt::{DeoptFrame, DeoptFrameEntry};
        let value = NumericValue;
        let hir = NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 1,
            parameter_count: 2,
            register_count: 2,
            arithmetic_op_count: 0,
            nodes: vec![
                NumericNode::Parameter {
                    register: 0,
                    value_type: NumericType::Tagged,
                },
                NumericNode::Parameter {
                    register: 1,
                    value_type: NumericType::Tagged,
                },
                NumericNode::TaggedToInt32(value(0)),
            ],
            blocks: vec![NumericBlock {
                logical_pc: 0,
                osr_entry_allowed: false,
                predecessors: vec![],
                successors: vec![],
                parameters: vec![],
                parameter_registers: vec![],
                successor_arguments: vec![],
                nodes: vec![value(0), value(1), value(2)],
                terminator: NumericTerminator::Return(value(2)),
            }],
            frame_states: vec![NumericFrameState {
                point: NumericFramePoint::Node(value(2)),
                frames: Box::new([
                    DeoptFrame {
                        function_id: 1,
                        byte_pc: 16,
                        entry: None,
                        slots: Box::new([
                            NumericFrameSlot::Value(value(0)),
                            NumericFrameSlot::Undefined,
                        ]),
                    },
                    DeoptFrame {
                        function_id: 2,
                        byte_pc: 8,
                        entry: Some(DeoptFrameEntry {
                            new_target: NumericFrameSlot::Undefined,
                            return_register: 1,
                            this: NumericFrameSlot::Value(value(0)),
                            closure: NumericFrameSlot::Value(value(1)),
                        }),
                        slots: Box::new([NumericFrameSlot::Value(value(0))]),
                    },
                ]),
            }],
            direct_call_targets: vec![],
            operand_values: vec![],
        };
        let sequence = select_with_packed_double_view_caches(
            &TargetSpec::aarch64(),
            &hir,
            &Default::default(),
        )
        .unwrap();
        let allocation = sequence.allocate(&TargetSpec::aarch64()).unwrap();
        let layout = crate::machine::MachineFrameLayout::new(&allocation, 0, 16, 16).unwrap();
        let table = lower_deopt_table(
            &sequence,
            &allocation,
            layout,
            16,
            8,
            &machine_frame_states(&hir),
        )
        .unwrap();
        let frames = &table.entries().next().unwrap().frames;
        assert_eq!(frames.len(), 2);
        let entry = frames[1].entry.as_ref().unwrap();
        assert_eq!(entry.this, frames[0].slots[0]);
        assert_ne!(entry.closure.location, entry.this.location);
        assert_eq!(entry.closure.repr, otter_vm::deopt::DeoptRepr::Tagged);
        let mut root_carrier = MachineInstruction::plain(MachineOpcode::Return, vec![]);
        attach_frame_state_tagged_roots(
            &hir,
            &[MachineValue(0), MachineValue(1), MachineValue(2)],
            0,
            &mut root_carrier,
        );
        assert_eq!(root_carrier.frame_state, Some(0));
        assert!(root_carrier.operands.is_empty());
    }

    use otter_bytecode::opcode_schema::{
        BindingRead, BindingSemantics, OPCODE_SCHEMA, OperandKind, RegisterAccess,
    };
    use otter_bytecode::{NO_HANDLER_OFFSET, Op, Operand};
    use otter_vm::{
        JitArtifactFileName, JitArtifactIdentity, JitCacheIrOp, JitCacheIrProgram,
        JitCompileSnapshot, JitDebugTarget, JitDebugTier, JitDirectCallThisMode, JitDirectCallee,
        JitFunctionCode, Value,
        jit::{BindingHitProof, JitDirectCallPlan, JitMethodGuard, JitTestInstruction},
        jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ARITH_STRING, ArithFeedback},
        native_abi::{
            NativeFrame, NativeFrameFlags, NativeFrameKind, NativeResultDomain, NativeResultPair,
            NativeResultStatus, VmFrameHeader, VmThread,
        },
        value::tag,
    };

    use super::*;
    use crate::entry::{JitCtx, JitEntry};
    use crate::machine::{
        AllocatedLocation, OperandConstraint, OperandPurpose, OperandTiming, SafepointId,
        VerificationError, lower_deopt_table,
    };

    const POLL_BATCH: i32 = crate::arm64::GENERATED_POLL_BATCH as i32;

    fn compiled_payload_bits(result: NativeResultPair) -> u64 {
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        result.payload_bits()
    }

    fn edge_conversion_hir(
        source_type: NumericType,
        target_type: NumericType,
    ) -> (NumericFunction, hir::NumericValue, hir::NumericValue) {
        let value = hir::NumericValue;
        let mut nodes = Vec::new();
        let source = match source_type {
            NumericType::Tagged => {
                nodes.push(NumericNode::TaggedConstant(Value::undefined().to_bits()));
                value(0)
            }
            NumericType::Int32 => {
                nodes.push(NumericNode::IntegerConstant(7));
                value(0)
            }
            NumericType::Uint32 => {
                nodes.extend([
                    NumericNode::IntegerConstant(-1),
                    NumericNode::IntegerConstant(0),
                    NumericNode::IntegerShiftRightLogical(value(0), value(1)),
                ]);
                value(2)
            }
            NumericType::Number => {
                nodes.push(NumericNode::Constant(7.5));
                value(0)
            }
            NumericType::Boolean => {
                nodes.push(NumericNode::BooleanConstant(true));
                value(0)
            }
        };
        let source_nodes = (0..nodes.len()).map(value).collect::<Vec<_>>();
        let parameter = value(nodes.len());
        nodes.push(NumericNode::BlockParameter(target_type));
        (
            NumericFunction {
                property_sites: BTreeMap::new(),
                constructor_field_sites: BTreeMap::new(),
                function_id: 175,
                nodes,
                blocks: vec![
                    hir::NumericBlock {
                        logical_pc: 0,
                        osr_entry_allowed: true,
                        predecessors: Vec::new(),
                        successors: vec![1],
                        parameters: Vec::new(),
                        parameter_registers: Vec::new(),
                        successor_arguments: vec![vec![source]],
                        nodes: source_nodes,
                        terminator: NumericTerminator::Jump,
                    },
                    hir::NumericBlock {
                        logical_pc: 1,
                        osr_entry_allowed: true,
                        predecessors: vec![0],
                        successors: Vec::new(),
                        parameters: vec![parameter],
                        parameter_registers: vec![0],
                        successor_arguments: Vec::new(),
                        nodes: vec![parameter],
                        terminator: NumericTerminator::Return(parameter),
                    },
                ],
                frame_states: Vec::new(),
                direct_call_targets: Vec::new(),
                operand_values: Vec::new(),
                parameter_count: 0,
                register_count: 1,
                arithmetic_op_count: 0,
            },
            source,
            parameter,
        )
    }

    fn tagged_backedge_conversion_hir() -> NumericFunction {
        let value = hir::NumericValue;
        NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 176,
            nodes: vec![
                NumericNode::TaggedConstant(Value::undefined().to_bits()),
                NumericNode::BlockParameter(NumericType::Tagged),
                NumericNode::IntegerConstant(1),
            ],
            blocks: vec![
                hir::NumericBlock {
                    logical_pc: 0,
                    osr_entry_allowed: true,
                    predecessors: Vec::new(),
                    successors: vec![1],
                    parameters: Vec::new(),
                    parameter_registers: Vec::new(),
                    successor_arguments: vec![vec![value(0)]],
                    nodes: vec![value(0)],
                    terminator: NumericTerminator::Jump,
                },
                hir::NumericBlock {
                    logical_pc: 1,
                    osr_entry_allowed: true,
                    predecessors: vec![0, 2],
                    successors: vec![2],
                    parameters: vec![value(1)],
                    parameter_registers: vec![0],
                    successor_arguments: vec![Vec::new()],
                    nodes: vec![value(1)],
                    terminator: NumericTerminator::Jump,
                },
                hir::NumericBlock {
                    logical_pc: 2,
                    osr_entry_allowed: true,
                    predecessors: vec![1],
                    successors: vec![1],
                    parameters: Vec::new(),
                    parameter_registers: Vec::new(),
                    successor_arguments: vec![vec![value(2)]],
                    nodes: vec![value(2)],
                    terminator: NumericTerminator::Jump,
                },
            ],
            frame_states: vec![hir::NumericFrameState {
                point: NumericFramePoint::Backedge {
                    predecessor: 2,
                    edge: 0,
                },
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 176,
                    byte_pc: 16,
                    entry: None,
                    slots: (vec![hir::NumericFrameSlot::Value(value(2))]).into(),
                }]),
            }],
            direct_call_targets: Vec::new(),
            operand_values: Vec::new(),
            parameter_count: 0,
            register_count: 1,
            arithmetic_op_count: 0,
        }
    }

    fn selection_direct_callee(function_id: u32) -> JitDirectCallee {
        JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id,
                code_object_id: u64::from(function_id) + 1,
                entry_cell: u64::from(function_id) + 2,
                tier: NativeFrameKind::Baseline,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
                is_derived_constructor: false,
                generated_stack_frame_bytes: Some(0),
                param_count: 1,
                register_count: 2,
                own_upvalue_count: 0,
                inherited_upvalue_count: 0,
                needs_incoming_arguments: false,
            },
            receiver_allocation: None,
        }
    }

    fn method_call_selection_hir() -> NumericFunction {
        let value = hir::NumericValue;
        NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 150,
            nodes: vec![
                NumericNode::Parameter {
                    register: 0,
                    value_type: NumericType::Int32,
                },
                NumericNode::BooleanConstant(true),
                NumericNode::DirectCall {
                    source: value(0),
                    target: 0,
                    arguments: NumericDirectCallArguments::Fixed { start: 0, count: 1 },
                    logical_pc: 4,
                    byte_pc: 32,
                    exceptional_edge: None,
                },
            ],
            blocks: vec![hir::NumericBlock {
                logical_pc: 0,
                osr_entry_allowed: true,
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                parameter_registers: Vec::new(),
                successor_arguments: Vec::new(),
                nodes: vec![value(0), value(1), value(2)],
                terminator: NumericTerminator::Return(value(2)),
            }],
            frame_states: vec![hir::NumericFrameState {
                point: NumericFramePoint::Node(value(2)),
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 150,
                    byte_pc: 32,
                    entry: None,
                    slots: (vec![
                        hir::NumericFrameSlot::Value(value(0)),
                        hir::NumericFrameSlot::Value(value(1)),
                        hir::NumericFrameSlot::Undefined,
                    ])
                    .into(),
                }]),
            }],
            direct_call_targets: vec![NumericDirectCallTarget {
                kind: NumericDirectCallKind::Method,
                candidates: (0..4)
                    .map(|target_index| {
                        let function_id = 160 + target_index;
                        hir::NumericDirectCallCandidate {
                            target_index,
                            target_count: 4,
                            guard: Some(JitMethodGuard {
                                method_fid: function_id,
                                recv_shape: 20 + target_index,
                                proto_chain: vec![30 + target_index],
                                method_value_byte: 40 + target_index * 8,
                            }),
                            callee: selection_direct_callee(function_id),
                        }
                    })
                    .collect(),
            }],
            operand_values: vec![value(1)],
            parameter_count: 1,
            register_count: 3,
            arithmetic_op_count: 0,
        }
    }

    fn generic_wide_method_call_selection_hir(argument_count: u32) -> NumericFunction {
        let value = hir::NumericValue;
        NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 152,
            nodes: vec![
                NumericNode::Parameter {
                    register: 0,
                    value_type: NumericType::Tagged,
                },
                NumericNode::DirectCall {
                    source: value(0),
                    target: 0,
                    arguments: NumericDirectCallArguments::Fixed {
                        start: 0,
                        count: argument_count,
                    },
                    logical_pc: 6,
                    byte_pc: 48,
                    exceptional_edge: None,
                },
            ],
            blocks: vec![hir::NumericBlock {
                logical_pc: 0,
                osr_entry_allowed: true,
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                parameter_registers: Vec::new(),
                successor_arguments: Vec::new(),
                nodes: vec![value(0), value(1)],
                terminator: NumericTerminator::Return(value(1)),
            }],
            frame_states: vec![hir::NumericFrameState {
                point: NumericFramePoint::Node(value(1)),
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 152,
                    byte_pc: 48,
                    entry: None,
                    slots: (vec![
                        hir::NumericFrameSlot::Value(value(0)),
                        hir::NumericFrameSlot::Undefined,
                    ])
                    .into(),
                }]),
            }],
            direct_call_targets: vec![NumericDirectCallTarget {
                kind: NumericDirectCallKind::Method,
                candidates: Vec::new(),
            }],
            operand_values: vec![
                value(0);
                usize::try_from(argument_count).expect("test argument count")
            ],
            parameter_count: 1,
            register_count: 2,
            arithmetic_op_count: 0,
        }
    }

    fn cold_call_selection_hir() -> NumericFunction {
        let value = hir::NumericValue;
        NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 151,
            nodes: vec![
                NumericNode::Parameter {
                    register: 0,
                    value_type: NumericType::Int32,
                },
                NumericNode::BooleanConstant(true),
                NumericNode::ColdCallExit {
                    kind: NumericColdCallKind::Plain,
                    logical_pc: 5,
                    byte_pc: 40,
                    exceptional_edge: None,
                },
            ],
            blocks: vec![hir::NumericBlock {
                logical_pc: 0,
                osr_entry_allowed: true,
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                parameter_registers: Vec::new(),
                successor_arguments: Vec::new(),
                nodes: vec![value(0), value(1), value(2)],
                terminator: NumericTerminator::Return(value(2)),
            }],
            frame_states: vec![hir::NumericFrameState {
                point: NumericFramePoint::Node(value(2)),
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 151,
                    byte_pc: 40,
                    entry: None,
                    slots: (vec![
                        hir::NumericFrameSlot::Value(value(0)),
                        hir::NumericFrameSlot::Value(value(1)),
                        hir::NumericFrameSlot::Undefined,
                    ])
                    .into(),
                }]),
            }],
            direct_call_targets: Vec::new(),
            operand_values: Vec::new(),
            parameter_count: 1,
            register_count: 3,
            arithmetic_op_count: 0,
        }
    }

    #[test]
    fn literal_selection_exposes_span_roots_and_rejects_invalid_contracts() {
        use otter_vm::native_abi::{STUB_JIT_NEW_ARRAY, STUB_JIT_NEW_OBJECT};
        for (target, count) in [
            (STUB_JIT_NEW_OBJECT, 0),
            (STUB_JIT_NEW_ARRAY, 0),
            (STUB_JIT_NEW_ARRAY, 3),
        ] {
            let mut hir = array_construct_selection_hir();
            hir.nodes[0] = NumericNode::TaggedConstant(otter_vm::Value::number_i32(7).to_bits());
            hir.nodes[1] = NumericNode::LiteralAllocation {
                target,
                argument_start: 0,
                argument_count: count,
                logical_pc: 1,
                byte_pc: 24,
            };
            hir.operand_values = vec![hir::NumericValue(0); count as usize];
            let sequence = select(&hir).expect("literal Machine selection");
            let (id, call) = sequence
                .instructions()
                .iter()
                .enumerate()
                .find(|(_, instruction)| matches!(instruction.opcode, MachineOpcode::Call(_)))
                .expect("literal call");
            assert!(call.exits.is_empty());
            assert_eq!(
                call.operands
                    .iter()
                    .filter(|operand| operand.purpose == OperandPurpose::Input)
                    .count(),
                count as usize
            );
            if count > 0 {
                assert_eq!(
                    call.operands
                        .iter()
                        .filter(|operand| operand.purpose == OperandPurpose::TaggedRoot)
                        .count(),
                    1,
                    "aliased elements share one rewriteable root"
                );
            }
            assert_eq!(
                value_packet_frame(&sequence)
                    .expect("literal frame")
                    .raw_words,
                count as u16
            );
            let allocation = sequence
                .allocate(&TargetSpec::aarch64())
                .expect("literal allocation");
            let table = lower_safepoints(&sequence, &allocation).expect("literal root table");
            assert_eq!(table.records().len(), 1);
            let mut invalid = sequence.clone();
            invalid.instructions[id].safepoint = None;
            assert!(
                invalid.verify(&TargetSpec::aarch64()).is_err(),
                "allocation cannot omit its safepoint"
            );
            if count > 0 {
                let mut invalid = sequence.clone();
                let MachineOpcode::Call(descriptor) = call.opcode else {
                    unreachable!()
                };
                invalid.call_descriptors[descriptor as usize].target =
                    CallTarget::LiteralAllocation {
                        target: STUB_JIT_NEW_OBJECT,
                        logical_pc: 1,
                        byte_pc: 24,
                    };
                assert!(
                    invalid.verify(&TargetSpec::aarch64()).is_err(),
                    "object allocation cannot consume array elements"
                );
            }
        }
    }

    fn array_construct_selection_hir() -> NumericFunction {
        let value = hir::NumericValue;
        NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 152,
            nodes: vec![
                NumericNode::IntegerConstant(7),
                NumericNode::ArrayConstruct {
                    length: value(0),
                    byte_pc: 24,
                },
            ],
            blocks: vec![hir::NumericBlock {
                logical_pc: 0,
                osr_entry_allowed: true,
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                parameter_registers: Vec::new(),
                successor_arguments: Vec::new(),
                nodes: vec![value(0), value(1)],
                terminator: NumericTerminator::Return(value(1)),
            }],
            frame_states: vec![hir::NumericFrameState {
                point: NumericFramePoint::Node(value(1)),
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 152,
                    byte_pc: 24,
                    entry: None,
                    slots: (vec![
                        hir::NumericFrameSlot::Value(value(0)),
                        hir::NumericFrameSlot::Undefined,
                    ])
                    .into(),
                }]),
            }],
            direct_call_targets: Vec::new(),
            operand_values: Vec::new(),
            parameter_count: 0,
            register_count: 2,
            arithmetic_op_count: 0,
        }
    }

    fn committed_value_selection_hir(local_catch: bool) -> NumericFunction {
        let value = hir::NumericValue;
        let committed = NumericNode::CommittedValue {
            operation: CommittedValueOperation::Scalar(
                otter_vm::native_abi::ScalarValueOp::SameValue,
            ),
            inputs: [Some(value(0)), Some(value(1))],
            logical_pc: 6,
            byte_pc: 48,
            exceptional_edge: local_catch.then_some(1),
        };
        let mut nodes = vec![
            NumericNode::Parameter {
                register: 0,
                value_type: NumericType::Tagged,
            },
            NumericNode::Parameter {
                register: 1,
                value_type: NumericType::Tagged,
            },
            NumericNode::Parameter {
                register: 2,
                value_type: NumericType::Tagged,
            },
            committed,
        ];
        let blocks = if local_catch {
            nodes.push(NumericNode::BlockParameter(NumericType::Tagged));
            vec![
                hir::NumericBlock {
                    logical_pc: 6,
                    osr_entry_allowed: false,
                    predecessors: Vec::new(),
                    successors: vec![1, 2],
                    parameters: Vec::new(),
                    parameter_registers: Vec::new(),
                    successor_arguments: vec![Vec::new(), vec![value(3)]],
                    nodes: vec![value(0), value(1), value(2), value(3)],
                    terminator: NumericTerminator::Jump,
                },
                hir::NumericBlock {
                    logical_pc: 7,
                    osr_entry_allowed: true,
                    predecessors: vec![0],
                    successors: Vec::new(),
                    parameters: Vec::new(),
                    parameter_registers: Vec::new(),
                    successor_arguments: Vec::new(),
                    nodes: Vec::new(),
                    terminator: NumericTerminator::Return(value(3)),
                },
                hir::NumericBlock {
                    logical_pc: 9,
                    osr_entry_allowed: true,
                    predecessors: vec![0],
                    successors: Vec::new(),
                    parameters: vec![value(4)],
                    parameter_registers: vec![3],
                    successor_arguments: Vec::new(),
                    nodes: vec![value(4)],
                    terminator: NumericTerminator::Return(value(4)),
                },
            ]
        } else {
            vec![hir::NumericBlock {
                logical_pc: 0,
                osr_entry_allowed: true,
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                parameter_registers: Vec::new(),
                successor_arguments: Vec::new(),
                nodes: vec![value(0), value(1), value(2), value(3)],
                terminator: NumericTerminator::Return(value(3)),
            }]
        };
        NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 153,
            nodes,
            blocks,
            frame_states: vec![hir::NumericFrameState {
                point: NumericFramePoint::Node(value(3)),
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 153,
                    byte_pc: 48,
                    entry: None,
                    slots: (vec![
                        hir::NumericFrameSlot::Value(value(0)),
                        hir::NumericFrameSlot::Value(value(1)),
                        hir::NumericFrameSlot::Value(value(2)),
                        hir::NumericFrameSlot::Undefined,
                    ])
                    .into(),
                }]),
            }],
            direct_call_targets: Vec::new(),
            operand_values: Vec::new(),
            parameter_count: 3,
            register_count: 4,
            arithmetic_op_count: 0,
        }
    }

    fn numeric_view(
        param_count: u16,
        register_count: u16,
        instructions: Vec<(Op, Vec<Operand>)>,
    ) -> JitCompileSnapshot {
        let mut view = JitCompileSnapshot::without_feedback(
            71,
            param_count,
            register_count,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 8, operands)
                })
                .collect(),
        );
        for pc in 0..view.instructions.len() {
            if matches!(
                view.instructions[pc].op(view.code_block.as_ref()),
                Op::Add
                    | Op::Sub
                    | Op::Mul
                    | Op::Div
                    | Op::Rem
                    | Op::Pow
                    | Op::Neg
                    | Op::Equal
                    | Op::NotEqual
                    | Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Increment
                    | Op::AddImm
                    | Op::SubImm
                    | Op::BitwiseAndImm
                    | Op::LessThanImm
                    | Op::EqualImm
                    | Op::NotEqualImm
            ) {
                view.seed_arith_feedback_for_test(
                    pc as u32,
                    ArithFeedback::from_bits(ARITH_INT32 | ARITH_FLOAT64),
                );
            }
        }
        view
    }

    fn identity_view() -> JitCompileSnapshot {
        let mut instructions = Vec::new();
        let mut source = 0;
        for destination in 1..=8 {
            instructions.push((
                Op::Neg,
                vec![Operand::Register(destination), Operand::Register(source)],
            ));
            source = destination;
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(source)]));
        numeric_view(1, 9, instructions)
    }

    fn overflow_view() -> JitCompileSnapshot {
        let mut instructions = vec![(Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)])];
        let mut source = 0;
        for destination in 2..=9 {
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(destination),
                    Operand::Register(source),
                    Operand::Register(1),
                ],
            ));
            source = destination;
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(source)]));
        numeric_view(1, 10, instructions)
    }

    fn typed_parameter_leaf_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            2,
            10,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(5),
                        Operand::Register(4),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(7),
                        Operand::Register(6),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Div,
                    vec![
                        Operand::Register(8),
                        Operand::Register(7),
                        Operand::Register(1),
                    ],
                ),
                (Op::Neg, vec![Operand::Register(9), Operand::Register(8)]),
                (Op::ReturnValue, vec![Operand::Register(9)]),
            ],
        );
        for pc in 0..=5 {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn typed_parameter_overflow_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            2,
            3,
            vec![
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(0, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn typed_parameter_leaf_overflow_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            1,
            7,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(5)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(2)]),
                (
                    Op::Div,
                    vec![
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::Rem,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(2),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(1)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(6),
                        Operand::Register(0),
                        Operand::Register(5),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(6)]),
            ],
        );
        view.seed_arith_feedback_for_test(5, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn typed_parameter_alias_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            1,
            4,
            vec![
                (
                    Op::StoreLocal,
                    vec![Operand::Register(0), Operand::Imm32(2)],
                ),
                (Op::LoadLocal, vec![Operand::Register(1), Operand::Imm32(2)]),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        );
        view.seed_arith_feedback_for_test(2, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn typed_parameter_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            2,
            8,
            vec![
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(5), Operand::Register(4)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(5),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(5), Operand::Imm32(2)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(6),
                        Operand::Register(3),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(6), Operand::Imm32(3)],
                ),
                (Op::Jump, vec![Operand::Imm32(-7)]),
                (Op::LoadLocal, vec![Operand::Register(7), Operand::Imm32(2)]),
                (Op::ReturnValue, vec![Operand::Register(7)]),
            ],
        );
        for pc in [2_u32, 4, 6] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn small_leaf_view() -> JitCompileSnapshot {
        numeric_view(
            1,
            2,
            vec![
                (Op::Neg, vec![Operand::Register(1), Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        )
    }

    fn tagged_identity_view() -> JitCompileSnapshot {
        numeric_view(1, 1, vec![(Op::ReturnValue, vec![Operand::Register(0)])])
    }

    fn tagged_immediate_view(op: Op) -> JitCompileSnapshot {
        assert!(matches!(op, Op::LoadUndefined | Op::LoadNull));
        numeric_view(
            0,
            1,
            vec![
                (op, vec![Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        )
    }

    fn tagged_this_view() -> JitCompileSnapshot {
        numeric_view(
            0,
            1,
            vec![
                (Op::LoadThis, vec![Operand::Register(0)]),
                (Op::Return, vec![Operand::Register(0)]),
            ],
        )
    }

    fn tagged_phi_view() -> JitCompileSnapshot {
        numeric_view(
            4,
            6,
            vec![
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(4)],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(0), Operand::Imm32(5)],
                ),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(1), Operand::Imm32(5)],
                ),
                (Op::ReturnValue, vec![Operand::Register(5)]),
            ],
        )
    }

    fn tagged_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            2,
            5,
            vec![
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(4)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(2),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-4)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        for pc in [2_u32, 4] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn tagged_truthiness_branch_view() -> JitCompileSnapshot {
        numeric_view(
            3,
            3,
            vec![
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(1), Operand::Register(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    fn tagged_logical_not_view() -> JitCompileSnapshot {
        numeric_view(
            1,
            2,
            vec![
                (
                    Op::LogicalNot,
                    vec![Operand::Register(1), Operand::Register(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        )
    }

    fn tagged_strict_equality_view(op: Op) -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(
            72,
            2,
            3,
            vec![
                JitTestInstruction::new(
                    op,
                    0,
                    0,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(2)]),
            ],
        )
    }

    fn tagged_mixed_strict_equality_view() -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(
            73,
            1,
            3,
            vec![
                JitTestInstruction::new(
                    Op::LoadInt32,
                    0,
                    0,
                    vec![Operand::Register(1), Operand::Imm32(7)],
                ),
                JitTestInstruction::new(
                    Op::Equal,
                    1,
                    8,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 2, 16, vec![Operand::Register(2)]),
            ],
        )
    }

    fn tagged_string_concat_view(parameter_count: u16) -> JitCompileSnapshot {
        assert!(parameter_count >= 2);
        let accumulator = parameter_count;
        let mut instructions = Vec::with_capacity(usize::from(parameter_count));
        instructions.push((
            Op::Add,
            vec![
                Operand::Register(accumulator),
                Operand::Register(0),
                Operand::Register(1),
            ],
        ));
        for parameter in 2..parameter_count {
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(accumulator),
                    Operand::Register(accumulator),
                    Operand::Register(parameter),
                ],
            ));
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(accumulator)]));
        let mut view = JitCompileSnapshot::without_feedback(
            74,
            parameter_count,
            parameter_count + 1,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 8, operands)
                })
                .collect(),
        );
        for pc in 0..u32::from(parameter_count - 1) {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_STRING));
        }
        view
    }

    fn unused_parameter_view() -> JitCompileSnapshot {
        numeric_view(
            2,
            3,
            vec![
                (Op::Neg, vec![Operand::Register(2), Operand::Register(1)]),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    fn float_bitwise_view(op: Op) -> JitCompileSnapshot {
        assert!(matches!(
            op,
            Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor | Op::Shl | Op::Shr | Op::Ushr
        ));
        numeric_view(
            2,
            3,
            vec![
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    fn boolean_bitwise_view() -> JitCompileSnapshot {
        numeric_view(
            0,
            4,
            vec![
                (Op::Nop, vec![]),
                (Op::LoadTrue, vec![Operand::Register(0)]),
                (Op::LoadFalse, vec![Operand::Register(1)]),
                (
                    Op::BitwiseAndImm,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Imm32(3),
                    ],
                ),
                (
                    Op::BitwiseOr,
                    vec![
                        Operand::Register(3),
                        Operand::Register(2),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        )
    }

    fn spill_pressure_view() -> JitCompileSnapshot {
        let mut instructions = (1..=32)
            .map(|register| {
                (
                    Op::LoadInt32,
                    vec![
                        Operand::Register(register),
                        Operand::Imm32(i32::from(register)),
                    ],
                )
            })
            .collect::<Vec<_>>();
        let mut source = 0;
        let mut destination = 33;
        for right in 1..=32 {
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(destination),
                    Operand::Register(source),
                    Operand::Register(right),
                ],
            ));
            source = destination;
            destination += 1;
        }
        instructions.push((Op::ReturnValue, vec![Operand::Register(source)]));
        numeric_view(1, destination, instructions)
    }

    fn diamond_view(branch: Op) -> JitCompileSnapshot {
        assert!(matches!(branch, Op::JumpIfTrue | Op::JumpIfFalse));
        numeric_view(
            2,
            4,
            vec![
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (branch, vec![Operand::Imm32(2), Operand::Register(2)]),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(1)]),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        )
    }

    fn critical_edge_view() -> JitCompileSnapshot {
        numeric_view(
            2,
            4,
            vec![
                (Op::LoadLocal, vec![Operand::Register(3), Operand::Imm32(0)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(0)]),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        )
    }

    fn loop_view() -> JitCompileSnapshot {
        numeric_view(
            1,
            3,
            vec![
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(2)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(0),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::Jump, vec![Operand::Imm32(-4)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        )
    }

    fn branch_phi_loop_view() -> JitCompileSnapshot {
        branch_phi_loop_view_with(0, 0, 1_000_000, 1)
    }

    fn countdown_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            7,
            vec![
                (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(5)]),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
                (
                    Op::NotEqualImm,
                    vec![
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::Imm32(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(7), Operand::Register(4)],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(5),
                        Operand::Register(0),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(5), Operand::Imm32(0)],
                ),
                (
                    Op::SubImm,
                    vec![
                        Operand::Register(6),
                        Operand::Register(1),
                        Operand::Imm32(-3),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(6), Operand::Imm32(1)],
                ),
                (
                    Op::Increment,
                    vec![
                        Operand::Register(6),
                        Operand::Register(2),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(6), Operand::Imm32(2)],
                ),
                (Op::Jump, vec![Operand::Imm32(-9)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        for pc in [4_u32, 6, 8, 10] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn bitwise_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            13,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(0x1234_5678)],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(2), Operand::Imm32(-1)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(i32::MAX)],
                ),
                (
                    Op::LessThanImm,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Imm32(35),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(10), Operand::Register(4)],
                ),
                (
                    Op::Shl,
                    vec![
                        Operand::Register(5),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::Shr,
                    vec![
                        Operand::Register(7),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::BitwiseXor,
                    vec![
                        Operand::Register(8),
                        Operand::Register(5),
                        Operand::Register(7),
                    ],
                ),
                (
                    Op::BitwiseOr,
                    vec![
                        Operand::Register(9),
                        Operand::Register(8),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::BitwiseAnd,
                    vec![
                        Operand::Register(10),
                        Operand::Register(9),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::BitwiseNot,
                    vec![Operand::Register(11), Operand::Register(10)],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(11), Operand::Imm32(0)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(12),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(12), Operand::Imm32(1)],
                ),
                (Op::Jump, vec![Operand::Imm32(-12)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        for pc in [4_u32, 13] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn checked_binary_view(op: Op, left: i32, right: i32) -> JitCompileSnapshot {
        assert!(matches!(op, Op::Add | Op::Sub | Op::Mul));
        let mut view = numeric_view(
            0,
            3,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(left)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(right)],
                ),
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(2, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn checked_immediate_view(op: Op, source: i32, immediate: i32) -> JitCompileSnapshot {
        assert!(matches!(op, Op::SubImm | Op::Increment));
        let mut view = numeric_view(
            0,
            2,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(source)],
                ),
                (
                    op,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::Imm32(immediate),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        view.seed_arith_feedback_for_test(1, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn checked_neg_view(source: i32) -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            2,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(source)],
                ),
                (Op::Neg, vec![Operand::Register(1), Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        view.seed_arith_feedback_for_test(1, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn float_binary_view(op: Op) -> JitCompileSnapshot {
        assert!(matches!(op, Op::Rem | Op::Pow));
        let mut view = numeric_view(
            2,
            3,
            vec![
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(0, ArithFeedback::from_bits(ARITH_FLOAT64));
        view
    }

    fn float_truthiness_view() -> JitCompileSnapshot {
        numeric_view(
            1,
            4,
            vec![
                (
                    Op::ToNumber,
                    vec![Operand::Register(1), Operand::Register(0)],
                ),
                (
                    Op::ToBoolean,
                    vec![Operand::Register(2), Operand::Register(1)],
                ),
                (
                    Op::LogicalNot,
                    vec![Operand::Register(3), Operand::Register(2)],
                ),
                (Op::ReturnValue, vec![Operand::Register(3)]),
            ],
        )
    }

    fn integer_truthiness_view(value: i32) -> JitCompileSnapshot {
        numeric_view(
            0,
            3,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(value)],
                ),
                (
                    Op::ToBoolean,
                    vec![Operand::Register(1), Operand::Register(0)],
                ),
                (
                    Op::LogicalNot,
                    vec![Operand::Register(2), Operand::Register(1)],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    fn ushr_view(left: i32, shift: i32) -> JitCompileSnapshot {
        numeric_view(
            0,
            3,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(left)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(shift)],
                ),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        )
    }

    fn ushr_comparison_view() -> JitCompileSnapshot {
        numeric_view(
            0,
            5,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(-1)],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(i32::MAX)],
                ),
                (
                    Op::GreaterThan,
                    vec![
                        Operand::Register(4),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(4)]),
            ],
        )
    }

    fn ushr_backedge_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            8,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(-1)],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(2), Operand::Imm32(0)],
                ),
                (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(3), Operand::Imm32(1)],
                ),
                (
                    Op::LessThanImm,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Imm32(POLL_BATCH + 4),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(5), Operand::Register(4)],
                ),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(5),
                        Operand::Register(0),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(5), Operand::Imm32(0)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(6),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(6), Operand::Imm32(1)],
                ),
                (Op::Jump, vec![Operand::Imm32(-7)]),
                (Op::ReturnValue, vec![Operand::Register(0)]),
            ],
        );
        for pc in [6_u32, 10] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn integer_comparison_view(op: Op, left: i32, right: i32) -> JitCompileSnapshot {
        assert!(matches!(
            op,
            Op::Equal | Op::NotEqual | Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq
        ));
        let mut view = numeric_view(
            0,
            3,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(left)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(1), Operand::Imm32(right)],
                ),
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(2, ArithFeedback::from_bits(ARITH_INT32));
        view
    }

    fn float_comparison_view(op: Op) -> JitCompileSnapshot {
        assert!(matches!(
            op,
            Op::Equal | Op::NotEqual | Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq
        ));
        let mut view = numeric_view(
            2,
            3,
            vec![
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(0, ArithFeedback::from_bits(ARITH_FLOAT64));
        view
    }

    fn integer_scalar_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            13,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(7), Operand::Imm32(-1)],
                ),
                (Op::LoadInt32, vec![Operand::Register(8), Operand::Imm32(0)]),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(0),
                        Operand::Register(7),
                        Operand::Register(8),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(2), Operand::Imm32(1_000_000)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(1023)],
                ),
                (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(3)]),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(7),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(15), Operand::Register(7)],
                ),
                (
                    Op::BitwiseAnd,
                    vec![
                        Operand::Register(8),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                (Op::LoadLocal, vec![Operand::Register(9), Operand::Imm32(4)]),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(5),
                        Operand::Register(8),
                        Operand::Register(9),
                    ],
                ),
                (
                    Op::BitwiseAndImm,
                    vec![
                        Operand::Register(6),
                        Operand::Register(1),
                        Operand::Imm32(7),
                    ],
                ),
                (
                    Op::BitwiseXor,
                    vec![
                        Operand::Register(8),
                        Operand::Register(0),
                        Operand::Register(5),
                    ],
                ),
                (Op::LoadLocal, vec![Operand::Register(9), Operand::Imm32(6)]),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(10),
                        Operand::Register(8),
                        Operand::Register(9),
                    ],
                ),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(11), Operand::Imm32(5)],
                ),
                (
                    Op::BitwiseOr,
                    vec![
                        Operand::Register(8),
                        Operand::Register(10),
                        Operand::Register(11),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(9), Operand::Imm32(0)]),
                (
                    Op::Ushr,
                    vec![
                        Operand::Register(10),
                        Operand::Register(8),
                        Operand::Register(9),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(10), Operand::Imm32(0)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(11),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(11), Operand::Imm32(1)],
                ),
                (Op::Jump, vec![Operand::Imm32(-17)]),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(12), Operand::Imm32(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(12)]),
            ],
        );
        for pc in [7_u32, 11, 12, 21] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn float_leaf_loop_view() -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            19,
            vec![
                (Op::LoadInt32, vec![Operand::Register(9), Operand::Imm32(1)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(10), Operand::Imm32(2)],
                ),
                (
                    Op::Div,
                    vec![
                        Operand::Register(0),
                        Operand::Register(9),
                        Operand::Register(10),
                    ],
                ),
                (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
                (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(1)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(200_000)],
                ),
                (
                    Op::LessThan,
                    vec![
                        Operand::Register(9),
                        Operand::Register(2),
                        Operand::Register(3),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(28), Operand::Register(9)],
                ),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(10), Operand::Imm32(2)],
                ),
                (
                    Op::ToPrimitive,
                    vec![
                        Operand::Register(11),
                        Operand::Register(10),
                        Operand::ConstIndex(1),
                    ],
                ),
                (Op::Neg, vec![Operand::Register(10), Operand::Register(11)]),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(10), Operand::Imm32(4)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(11), Operand::Imm32(17)],
                ),
                (
                    Op::Rem,
                    vec![
                        Operand::Register(5),
                        Operand::Register(4),
                        Operand::Register(11),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Register(0),
                    ],
                ),
                (
                    Op::Mul,
                    vec![
                        Operand::Register(11),
                        Operand::Register(6),
                        Operand::Register(6),
                    ],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(12), Operand::Imm32(1)],
                ),
                (
                    Op::Pow,
                    vec![
                        Operand::Register(7),
                        Operand::Register(11),
                        Operand::Register(12),
                    ],
                ),
                (
                    Op::Sub,
                    vec![
                        Operand::Register(11),
                        Operand::Register(7),
                        Operand::Register(7),
                    ],
                ),
                (
                    Op::ToNumber,
                    vec![Operand::Register(11), Operand::Register(11)],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(11), Operand::Imm32(8)],
                ),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(12), Operand::Imm32(8)],
                ),
                (
                    Op::LogicalNot,
                    vec![Operand::Register(12), Operand::Register(12)],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(12)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(13),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(13), Operand::Imm32(1)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(14), Operand::Imm32(13)],
                ),
                (
                    Op::Rem,
                    vec![
                        Operand::Register(15),
                        Operand::Register(7),
                        Operand::Register(14),
                    ],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(16), Operand::Imm32(1)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(17), Operand::Imm32(2)],
                ),
                (
                    Op::Div,
                    vec![
                        Operand::Register(18),
                        Operand::Register(16),
                        Operand::Register(17),
                    ],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(14),
                        Operand::Register(15),
                        Operand::Register(18),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(14), Operand::Imm32(0)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(15),
                        Operand::Register(2),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(15), Operand::Imm32(2)],
                ),
                (Op::Jump, vec![Operand::Imm32(-30)]),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(16), Operand::Imm32(1)],
                ),
                (Op::ReturnValue, vec![Operand::Register(16)]),
            ],
        );
        for pc in [6_u32, 10, 24, 33] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn float_bitwise_loop_view() -> JitCompileSnapshot {
        let r = Operand::Register;
        let i = Operand::Imm32;
        let c = Operand::ConstIndex;
        let mut view = numeric_view(
            0,
            12,
            vec![
                (Op::LoadNumber, vec![r(0), c(1)]),
                (Op::LoadInt32, vec![r(1), i(0)]),
                (Op::LoadInt32, vec![r(2), i(0)]),
                (Op::LoadInt32, vec![r(3), i(200_000)]),
                (Op::LessThan, vec![r(6), r(2), r(3)]),
                (Op::JumpIfFalse, vec![i(17), r(6)]),
                (Op::LoadInt32, vec![r(7), i(0)]),
                (Op::BitwiseOr, vec![r(4), r(0), r(7)]),
                (Op::LoadLocal, vec![r(7), i(0)]),
                (Op::BitwiseAndImm, vec![r(8), r(2), i(7)]),
                (Op::Ushr, vec![r(5), r(7), r(8)]),
                (Op::BitwiseXor, vec![r(7), r(1), r(4)]),
                (Op::LoadLocal, vec![r(8), i(5)]),
                (Op::BitwiseXor, vec![r(9), r(7), r(8)]),
                (Op::LoadInt32, vec![r(10), i(0)]),
                (Op::BitwiseOr, vec![r(7), r(9), r(10)]),
                (Op::StoreLocal, vec![r(7), i(1)]),
                (Op::LoadNumber, vec![r(8), c(2)]),
                (Op::Add, vec![r(9), r(0), r(8)]),
                (Op::StoreLocal, vec![r(9), i(0)]),
                (Op::AddImm, vec![r(10), r(2), i(1)]),
                (Op::StoreLocal, vec![r(10), i(2)]),
                (Op::Jump, vec![i(-19)]),
                (Op::LoadLocal, vec![r(11), i(1)]),
                (Op::ReturnValue, vec![r(11)]),
            ],
        );
        view.instructions[0].load_number = Some(4_294_967_297.75);
        view.instructions[17].load_number = Some(1.5);
        for pc in [4_u32, 20] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view.seed_arith_feedback_for_test(18, ArithFeedback::from_bits(ARITH_FLOAT64));
        view
    }

    fn mixed_osr_loop_view() -> JitCompileSnapshot {
        let r = Operand::Register;
        let i = Operand::Imm32;
        let mut view = numeric_view(
            0,
            10,
            vec![
                (Op::LoadInt32, vec![r(7), i(-1)]),
                (Op::LoadInt32, vec![r(8), i(0)]),
                (Op::Ushr, vec![r(0), r(7), r(8)]),
                (Op::LoadTrue, vec![r(1)]),
                (Op::LoadInt32, vec![r(2), i(0)]),
                (Op::LoadInt32, vec![r(3), i(3)]),
                (Op::LessThan, vec![r(4), r(2), r(3)]),
                (Op::JumpIfFalse, vec![i(8), r(4)]),
                (Op::LoadInt32, vec![r(5), i(1)]),
                (Op::Ushr, vec![r(6), r(0), r(5)]),
                (Op::StoreLocal, vec![r(6), i(0)]),
                (Op::LogicalNot, vec![r(7), r(1)]),
                (Op::StoreLocal, vec![r(7), i(1)]),
                (Op::AddImm, vec![r(8), r(2), i(1)]),
                (Op::StoreLocal, vec![r(8), i(2)]),
                (Op::Jump, vec![i(-10)]),
                (Op::ReturnValue, vec![r(0)]),
            ],
        );
        for pc in [6_u32, 13] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn osr_spill_pressure_loop_view() -> JitCompileSnapshot {
        const LIVE_VALUES: u16 = 30;
        let loop_index = LIVE_VALUES;
        let loop_limit = LIVE_VALUES + 1;
        let condition = LIVE_VALUES + 2;
        let accumulator = LIVE_VALUES + 3;
        let scratch = LIVE_VALUES + 4;
        let mut instructions = (0_u16..LIVE_VALUES)
            .map(|register| {
                (
                    Op::LoadInt32,
                    vec![
                        Operand::Register(register),
                        Operand::Imm32(i32::from(register) + 1),
                    ],
                )
            })
            .collect::<Vec<_>>();
        instructions.extend([
            (
                Op::LoadInt32,
                vec![Operand::Register(loop_index), Operand::Imm32(0)],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(loop_limit), Operand::Imm32(1)],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(accumulator), Operand::Imm32(0)],
            ),
            (
                Op::LessThan,
                vec![
                    Operand::Register(condition),
                    Operand::Register(loop_index),
                    Operand::Register(loop_limit),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![
                    Operand::Imm32(i32::from(LIVE_VALUES) * 2 + 3),
                    Operand::Register(condition),
                ],
            ),
        ]);
        for register in 0_u16..LIVE_VALUES {
            instructions.push((
                Op::Add,
                vec![
                    Operand::Register(scratch),
                    Operand::Register(accumulator),
                    Operand::Register(register),
                ],
            ));
            instructions.push((
                Op::StoreLocal,
                vec![
                    Operand::Register(scratch),
                    Operand::Imm32(i32::from(accumulator)),
                ],
            ));
        }
        instructions.extend([
            (
                Op::AddImm,
                vec![
                    Operand::Register(scratch),
                    Operand::Register(loop_index),
                    Operand::Imm32(1),
                ],
            ),
            (
                Op::StoreLocal,
                vec![
                    Operand::Register(scratch),
                    Operand::Imm32(i32::from(loop_index)),
                ],
            ),
            (
                Op::Jump,
                vec![Operand::Imm32(-(i32::from(LIVE_VALUES) * 2 + 5))],
            ),
            (Op::ReturnValue, vec![Operand::Register(accumulator)]),
        ]);
        let mut view = numeric_view(0, LIVE_VALUES + 5, instructions);
        let header_pc = u32::from(LIVE_VALUES + 3);
        let add_imm_pc = header_pc + u32::from(LIVE_VALUES) * 2 + 2;
        for pc in header_pc..=add_imm_pc {
            if pc == header_pc || (pc - header_pc >= 2 && (pc - header_pc).is_multiple_of(2)) {
                view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
            }
        }
        view
    }

    fn branch_phi_loop_view_with(
        initial_checksum: i32,
        initial_index: i32,
        limit: i32,
        increment: i32,
    ) -> JitCompileSnapshot {
        let mut view = numeric_view(
            0,
            12,
            vec![
                (
                    Op::LoadInt32,
                    vec![Operand::Register(0), Operand::Imm32(initial_checksum)],
                ),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(3), Operand::Imm32(initial_index)],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(3), Operand::Imm32(1)],
                ),
                (
                    Op::LessThanImm,
                    vec![
                        Operand::Register(4),
                        Operand::Register(1),
                        Operand::Imm32(limit),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(13), Operand::Register(4)],
                ),
                (
                    Op::BitwiseAndImm,
                    vec![
                        Operand::Register(5),
                        Operand::Register(1),
                        Operand::Imm32(1),
                    ],
                ),
                (
                    Op::EqualImm,
                    vec![
                        Operand::Register(6),
                        Operand::Register(5),
                        Operand::Imm32(0),
                    ],
                ),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(3), Operand::Register(6)],
                ),
                (Op::LoadInt32, vec![Operand::Register(7), Operand::Imm32(2)]),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(7), Operand::Imm32(2)],
                ),
                (Op::Jump, vec![Operand::Imm32(2)]),
                (
                    Op::LoadInt32,
                    vec![Operand::Register(8), Operand::Imm32(-14)],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(8), Operand::Imm32(2)],
                ),
                (
                    Op::Add,
                    vec![
                        Operand::Register(9),
                        Operand::Register(0),
                        Operand::Register(2),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(9), Operand::Imm32(0)],
                ),
                (
                    Op::AddImm,
                    vec![
                        Operand::Register(10),
                        Operand::Register(1),
                        Operand::Imm32(increment),
                    ],
                ),
                (
                    Op::StoreLocal,
                    vec![Operand::Register(10), Operand::Imm32(1)],
                ),
                (Op::Jump, vec![Operand::Imm32(-15)]),
                (
                    Op::LoadLocal,
                    vec![Operand::Register(11), Operand::Imm32(0)],
                ),
                (Op::ReturnValue, vec![Operand::Register(11)]),
            ],
        );
        for pc in [3_u32, 5, 6, 13, 15] {
            view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_INT32));
        }
        view
    }

    fn compile_output(
        view: &JitCompileSnapshot,
        artifact_request: Option<ArtifactRequest>,
    ) -> NativeCompileOutput<OptimizedCode> {
        let transitions = TransitionTable::resolve();
        compile_output_with_transitions(view, &transitions, artifact_request)
    }

    fn compile_output_with_transitions(
        view: &JitCompileSnapshot,
        transitions: &TransitionTable,
        artifact_request: Option<ArtifactRequest>,
    ) -> NativeCompileOutput<OptimizedCode> {
        try_compile(
            &TargetSpec::aarch64(),
            view,
            7001,
            transitions,
            false,
            artifact_request,
        )
        .expect("numeric Machine IR code generation")
    }

    fn execute(
        code: &OptimizedCode,
        args: &[u64],
        initial_pc: u32,
    ) -> (NativeResultPair, Vec<u64>, u32) {
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let result = execute_with_poll_cells(
            code,
            args,
            initial_pc,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        let mut original_frame =
            vec![Value::undefined().to_bits(); code.metadata().register_count as usize];
        original_frame[..args.len()].copy_from_slice(args);
        assert_eq!(
            result.1, original_frame,
            "successful numeric function must not mutate VM slots"
        );
        result
    }

    fn execute_with_poll_cells(
        code: &OptimizedCode,
        args: &[u64],
        initial_pc: u32,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (NativeResultPair, Vec<u64>, u32) {
        assert!(args.len() <= code.metadata().register_count as usize);
        let entry: JitEntry = unsafe { std::mem::transmute(code.compiled_code().entry_ptr()) };
        let mut frame = vec![Value::undefined().to_bits(); code.metadata().register_count as usize];
        frame[..args.len()].copy_from_slice(args);
        execute_at(code, entry, frame, initial_pc, interrupt, fuel)
    }

    fn execute_osr_with_poll_cells(
        code: &OptimizedCode,
        logical_pc: u32,
        frame: Vec<u64>,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (NativeResultPair, Vec<u64>, u32) {
        // SAFETY: the code object owns the recorded trampoline throughout the call.
        let entry = unsafe {
            code.osr_entry_ptr_for_test(logical_pc)
                .expect("numeric OSR entry")
        };
        // SAFETY: the trampoline uses the same shared `JitEntry` ABI as main entry.
        let entry: JitEntry = unsafe { std::mem::transmute(entry) };
        execute_at(code, entry, frame, logical_pc, interrupt, fuel)
    }

    fn execute_at(
        code: &OptimizedCode,
        entry: JitEntry,
        frame: Vec<u64>,
        initial_pc: u32,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (NativeResultPair, Vec<u64>, u32) {
        let register_count = code.metadata().register_count;
        let (result, frame, pc, _) = execute_at_with_register_count(
            code,
            entry,
            frame,
            initial_pc,
            register_count,
            Value::undefined(),
            interrupt,
            fuel,
        );
        (result, frame, pc)
    }

    fn execute_at_with_register_count(
        code: &OptimizedCode,
        entry: JitEntry,
        frame: Vec<u64>,
        initial_pc: u32,
        initialized_register_count: u16,
        this_value: Value,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (NativeResultPair, Vec<u64>, u32, u16) {
        let heap = otter_gc::GcHeap::new().expect("execution-test heap");
        execute_at_with_heap(
            code,
            entry,
            frame,
            initial_pc,
            initialized_register_count,
            this_value,
            std::ptr::from_ref(&heap),
            interrupt,
            fuel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_at_with_heap(
        code: &OptimizedCode,
        entry: JitEntry,
        mut frame: Vec<u64>,
        initial_pc: u32,
        initialized_register_count: u16,
        this_value: Value,
        heap: *const otter_gc::GcHeap,
        interrupt: *const u8,
        fuel: &mut u64,
    ) -> (NativeResultPair, Vec<u64>, u32, u16) {
        assert_eq!(frame.len(), code.metadata().register_count as usize);
        let metadata = code.metadata();
        let mut native_frame = NativeFrame::new(
            VmFrameHeader {
                function_id: metadata.function_id,
                pc: initial_pc,
                register_count: initialized_register_count,
                kind: NativeFrameKind::Optimizing,
                flags: NativeFrameFlags::empty(),
            },
            frame.as_mut_ptr() as u64,
            Value::undefined(),
            this_value,
        );
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.current_code_object_id = metadata.code_object_id;
        thread.interrupt_cell = interrupt as u64;
        thread.gc_heap = heap as u64;
        thread.backedge_fuel_cell = std::ptr::from_mut(fuel) as u64;
        let mut error = None;
        let mut machine_roots = 0;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(native_frame),
            error: &mut error,
            activation_base: std::ptr::null_mut(),
            activation_top_ptr: std::ptr::null_mut(),
            activation_limit: 0,
            machine_roots_ptr: std::ptr::addr_of_mut!(machine_roots),
            receiver_alloc: otter_vm::jit::JitMachineAllocationWindow::disabled(),
            runtime_stats: std::ptr::null_mut(),
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
        };
        let result = entry(&mut ctx);
        (
            result,
            frame,
            native_frame.header.pc,
            native_frame.header.register_count,
        )
    }

    fn boxed_f64(value: f64) -> u64 {
        Value::number_f64(value).to_bits()
    }

    fn unbox_number(bits: u64) -> f64 {
        if tag::is_int32_bits(bits) {
            f64::from(tag::unbox_int32(bits))
        } else {
            assert!(tag::is_double_bits(bits), "result must be a Number");
            f64::from_bits(tag::unbox_double(bits))
        }
    }

    fn explicit_element_selection_hir(
        access: Option<NumericElementAccess>,
        store: bool,
    ) -> NumericFunction {
        let value = hir::NumericValue;
        let element = if store {
            NumericNode::ElementStore {
                receiver: value(0),
                index: value(1),
                value: value(2),
                byte_pc: 24,
                access,
                exceptional_edge: None,
            }
        } else {
            NumericNode::ElementLoad {
                receiver: value(0),
                index: value(1),
                byte_pc: 24,
                access,
                exceptional_edge: None,
            }
        };
        NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 93,
            nodes: vec![
                NumericNode::Parameter {
                    register: 0,
                    value_type: NumericType::Tagged,
                },
                NumericNode::Parameter {
                    register: 1,
                    value_type: NumericType::Tagged,
                },
                NumericNode::BooleanConstant(true),
                element,
            ],
            blocks: vec![
                hir::NumericBlock {
                    logical_pc: 0,
                    osr_entry_allowed: true,
                    predecessors: vec![],
                    successors: vec![1],
                    parameters: vec![],
                    parameter_registers: vec![],
                    successor_arguments: vec![vec![]],
                    nodes: (0..4).map(value).collect(),
                    terminator: NumericTerminator::Jump,
                },
                hir::NumericBlock {
                    logical_pc: 1,
                    osr_entry_allowed: false,
                    predecessors: vec![0],
                    successors: vec![],
                    parameters: vec![],
                    parameter_registers: vec![],
                    successor_arguments: vec![],
                    nodes: vec![],
                    terminator: NumericTerminator::Return(if store { value(0) } else { value(3) }),
                },
            ],
            frame_states: vec![hir::NumericFrameState {
                point: NumericFramePoint::Node(value(3)),
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 93,
                    byte_pc: 24,
                    entry: None,
                    slots: vec![
                        hir::NumericFrameSlot::Value(value(0)),
                        hir::NumericFrameSlot::Value(value(1)),
                        hir::NumericFrameSlot::Value(value(2)),
                    ]
                    .into(),
                }]),
            }],
            direct_call_targets: Vec::new(),
            operand_values: Vec::new(),
            parameter_count: 2,
            register_count: 3,
            arithmetic_op_count: 0,
        }
    }

    pub(super) fn property_selection_hir() -> NumericFunction {
        let value = |index| hir::NumericValue(index);
        NumericFunction {
            constructor_field_sites: BTreeMap::new(),
            property_sites: [
                (
                    value(2),
                    super::super::MachineCacheIrSite {
                        function_id: 94,
                        logical_pc: 0,
                        byte_pc: 24,
                        program: vec![JitCacheIrProgram {
                            ops: vec![
                                JitCacheIrOp::GuardShape {
                                    object: 0,
                                    shape: 7,
                                },
                                JitCacheIrOp::LoadField {
                                    object: 0,
                                    value_byte: 8,
                                },
                            ]
                            .into_boxed_slice(),
                        }]
                        .into_boxed_slice(),
                    },
                ),
                (
                    value(3),
                    super::super::MachineCacheIrSite {
                        function_id: 94,
                        logical_pc: 1,
                        byte_pc: 40,
                        program: vec![JitCacheIrProgram {
                            ops: vec![
                                JitCacheIrOp::GuardShape {
                                    object: 0,
                                    shape: 7,
                                },
                                JitCacheIrOp::StoreField {
                                    object: 0,
                                    value_byte: 8,
                                },
                            ]
                            .into_boxed_slice(),
                        }]
                        .into_boxed_slice(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
            function_id: 94,
            nodes: vec![
                NumericNode::IntegerConstant(7),
                NumericNode::BooleanConstant(true),
                NumericNode::PropertyLoad {
                    receiver: value(0),
                    byte_pc: 24,
                    exotic_length: false,
                    exceptional_edge: None,
                },
                NumericNode::PropertyStore {
                    receiver: value(0),
                    value: value(1),
                    byte_pc: 40,
                },
            ],
            blocks: vec![
                hir::NumericBlock {
                    logical_pc: 0,
                    osr_entry_allowed: true,
                    predecessors: vec![],
                    successors: vec![1],
                    parameters: vec![],
                    parameter_registers: vec![],
                    successor_arguments: vec![vec![]],
                    nodes: (0..3).map(value).collect(),
                    terminator: NumericTerminator::Jump,
                },
                hir::NumericBlock {
                    logical_pc: 1,
                    osr_entry_allowed: false,
                    predecessors: vec![0],
                    successors: vec![2],
                    parameters: Vec::new(),
                    parameter_registers: Vec::new(),
                    successor_arguments: vec![vec![]],
                    nodes: vec![value(3)],
                    terminator: NumericTerminator::Jump,
                },
                hir::NumericBlock {
                    logical_pc: 2,
                    osr_entry_allowed: false,
                    predecessors: vec![1],
                    successors: vec![],
                    parameters: vec![],
                    parameter_registers: vec![],
                    successor_arguments: vec![],
                    nodes: vec![],
                    terminator: NumericTerminator::Return(value(2)),
                },
            ],
            frame_states: [
                hir::NumericFrameState {
                    point: NumericFramePoint::Node(value(2)),
                    frames: Box::new([otter_vm::deopt::DeoptFrame {
                        function_id: 94,
                        byte_pc: 24,
                        entry: None,
                        slots: (vec![
                            hir::NumericFrameSlot::Value(value(0)),
                            hir::NumericFrameSlot::Value(value(1)),
                            hir::NumericFrameSlot::Undefined,
                        ])
                        .into(),
                    }]),
                },
                hir::NumericFrameState {
                    point: NumericFramePoint::Node(value(3)),
                    frames: Box::new([otter_vm::deopt::DeoptFrame {
                        function_id: 94,
                        byte_pc: 40,
                        entry: None,
                        slots: (vec![
                            hir::NumericFrameSlot::Value(value(0)),
                            hir::NumericFrameSlot::Value(value(1)),
                            hir::NumericFrameSlot::Value(value(2)),
                        ])
                        .into(),
                    }]),
                },
            ]
            .into(),
            direct_call_targets: Vec::new(),
            operand_values: Vec::new(),
            parameter_count: 0,
            register_count: 3,
            arithmetic_op_count: 0,
        }
    }

    fn property_transition_selection_hir() -> NumericFunction {
        let mut hir = property_selection_hir();
        let site = hir::NumericValue(3);
        hir.property_sites.get_mut(&site).unwrap().program = vec![JitCacheIrProgram {
            ops: vec![
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: 7,
                },
                JitCacheIrOp::GuardPrototypeNull { object: 0 },
                JitCacheIrOp::GuardExtensible {
                    object: 0,
                    value_byte: 8,
                },
                JitCacheIrOp::StoreField {
                    object: 0,
                    value_byte: 8,
                },
                JitCacheIrOp::PublishShape {
                    object: 0,
                    shape: 11,
                    new_len: 2,
                    initialize_inline: false,
                },
            ]
            .into_boxed_slice(),
        }]
        .into_boxed_slice();
        hir
    }

    fn binding_selection_hir() -> NumericFunction {
        let mut view = JitCompileSnapshot::without_feedback(
            95,
            1,
            3,
            vec![
                JitTestInstruction::new(
                    Op::LoadGlobalOrThrow,
                    0,
                    24,
                    vec![Operand::Register(1), Operand::ConstIndex(0)],
                ),
                JitTestInstruction::new(
                    Op::LoadGlobalOrUndefined,
                    1,
                    32,
                    vec![Operand::Register(2), Operand::ConstIndex(1)],
                ),
                JitTestInstruction::new(Op::ReturnValue, 2, 40, vec![Operand::Register(2)]),
            ],
        );
        view.cage_base = 0x1000;
        view.binding_hit_proofs.insert(
            24,
            BindingHitProof::GlobalLexical {
                cell_offset: 0x1234,
                writable: true,
            },
        );
        view.binding_hit_proofs.insert(
            32,
            BindingHitProof::GlobalObject {
                shape: 0x5678,
                dictionary: true,
                value_byte: 40,
                global_lexical_epoch: 9,
                writable: true,
            },
        );
        NumericFunction::build(&view).expect("typed binding HIR")
    }

    fn schema_binding_hir(schema: &otter_bytecode::opcode_schema::OpcodeSchema) -> NumericFunction {
        let semantics = schema.binding.expect("binding schema row");
        let operands = schema
            .operand_shape
            .fixed()
            .expect("binding opcodes have fixed operands")
            .iter()
            .map(|operand| match operand.kind {
                OperandKind::Register => {
                    Operand::Register(if operand.register_access == RegisterAccess::Write {
                        1
                    } else {
                        0
                    })
                }
                OperandKind::ConstIndex => Operand::ConstIndex(0),
                OperandKind::Imm32 => Operand::Imm32(0),
            })
            .collect::<Vec<_>>();
        let terminator = semantics.result_operand().map_or_else(
            || JitTestInstruction::new(Op::ReturnUndefined, 1, 32, Vec::new()),
            |destination| {
                let Operand::Register(destination) = operands[usize::from(destination)] else {
                    panic!("binding result is a register operand")
                };
                JitTestInstruction::new(
                    Op::ReturnValue,
                    1,
                    32,
                    vec![Operand::Register(destination)],
                )
            },
        );
        let view = JitCompileSnapshot::without_feedback(
            196,
            1,
            2,
            vec![
                JitTestInstruction::new(schema.op, 0, 24, operands),
                terminator,
            ],
        );
        NumericFunction::build(&view).unwrap_or_else(|_| {
            panic!("schema binding {:?} must build cold Machine HIR", schema.op)
        })
    }

    fn binding_catch_hir() -> NumericFunction {
        let view = JitCompileSnapshot::without_feedback(
            197,
            0,
            3,
            vec![
                JitTestInstruction::new(
                    Op::EnterTry,
                    0,
                    0,
                    vec![
                        Operand::Imm32(3),
                        Operand::Imm32(NO_HANDLER_OFFSET),
                        Operand::Register(2),
                    ],
                ),
                JitTestInstruction::new(
                    Op::LoadGlobalOrThrow,
                    1,
                    8,
                    vec![Operand::Register(1), Operand::ConstIndex(0)],
                ),
                JitTestInstruction::new(Op::LeaveTry, 2, 16, Vec::new()),
                JitTestInstruction::new(Op::ReturnValue, 3, 24, vec![Operand::Register(1)]),
                JitTestInstruction::new(Op::ReturnValue, 4, 32, vec![Operand::Register(2)]),
            ],
        );
        NumericFunction::build(&view).expect("caught binding HIR")
    }

    fn binding_write_selection_hir() -> NumericFunction {
        let mut view = JitCompileSnapshot::without_feedback(
            198,
            0,
            2,
            vec![
                JitTestInstruction::new(
                    Op::LoadInt32,
                    0,
                    0,
                    vec![Operand::Register(0), Operand::Imm32(7)],
                ),
                JitTestInstruction::new(Op::LoadTrue, 1, 8, vec![Operand::Register(1)]),
                JitTestInstruction::new(
                    Op::StoreUpvalueChecked,
                    2,
                    16,
                    vec![Operand::Register(0), Operand::Imm32(0)],
                ),
                JitTestInstruction::new(
                    Op::StoreGlobalBinding,
                    3,
                    24,
                    vec![
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Imm32(1),
                    ],
                ),
                JitTestInstruction::new(
                    Op::StoreGlobalChecked,
                    4,
                    32,
                    vec![
                        Operand::Register(0),
                        Operand::ConstIndex(1),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnUndefined, 5, 40, Vec::new()),
            ],
        );
        view.cage_base = 0x1000;
        view.binding_hit_proofs.insert(
            24,
            BindingHitProof::GlobalLexical {
                cell_offset: 0x120,
                writable: true,
            },
        );
        view.binding_hit_proofs.insert(
            32,
            BindingHitProof::GlobalObject {
                shape: 11,
                dictionary: false,
                value_byte: 24,
                global_lexical_epoch: 4,
                writable: true,
            },
        );
        NumericFunction::build(&view).expect("generated binding-write HIR")
    }

    fn binding_followed_by_deopt_hir() -> NumericFunction {
        let mut view = JitCompileSnapshot::without_feedback(
            199,
            1,
            4,
            vec![
                JitTestInstruction::new(
                    Op::LoadGlobalOrThrow,
                    0,
                    8,
                    vec![Operand::Register(1), Operand::ConstIndex(0)],
                ),
                JitTestInstruction::new(
                    Op::LoadGlobalOrThrow,
                    1,
                    16,
                    vec![Operand::Register(2), Operand::ConstIndex(1)],
                ),
                JitTestInstruction::new(
                    Op::Add,
                    2,
                    24,
                    vec![
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                JitTestInstruction::new(
                    Op::Add,
                    3,
                    32,
                    vec![
                        Operand::Register(3),
                        Operand::Register(3),
                        Operand::Register(0),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 4, 40, vec![Operand::Register(3)]),
            ],
        );
        view.cage_base = 0x1000;
        view.binding_hit_proofs.insert(
            8,
            BindingHitProof::GlobalLexical {
                cell_offset: 0x140,
                writable: true,
            },
        );
        view.binding_hit_proofs.insert(
            16,
            BindingHitProof::GlobalObject {
                shape: 13,
                dictionary: false,
                value_byte: 32,
                global_lexical_epoch: 5,
                writable: true,
            },
        );
        NumericFunction::build(&view).expect("binding-to-deopt HIR")
    }

    fn string_constant_selection_hir() -> NumericFunction {
        let value = |index| hir::NumericValue(index);
        NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 96,
            nodes: vec![NumericNode::StringConstantCell {
                byte_pc: 24,
                target: otter_vm::jit::JitStringConstantCell { cell_addr: 0x1238 },
            }],
            blocks: vec![hir::NumericBlock {
                logical_pc: 0,
                osr_entry_allowed: true,
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                parameter_registers: Vec::new(),
                successor_arguments: Vec::new(),
                nodes: vec![value(0)],
                terminator: NumericTerminator::Return(value(0)),
            }],
            frame_states: Vec::new(),
            direct_call_targets: Vec::new(),
            operand_values: Vec::new(),
            parameter_count: 0,
            register_count: 1,
            arithmetic_op_count: 0,
        }
    }

    fn tagged_nullish_selection_hir(equal: bool) -> NumericFunction {
        let value = |index| hir::NumericValue(index);
        NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 96,
            nodes: vec![
                NumericNode::Parameter {
                    register: 0,
                    value_type: NumericType::Tagged,
                },
                NumericNode::TaggedNullishEqual {
                    value: value(0),
                    equal,
                    byte_pc: 24,
                },
            ],
            blocks: vec![hir::NumericBlock {
                logical_pc: 0,
                osr_entry_allowed: true,
                predecessors: Vec::new(),
                successors: Vec::new(),
                parameters: Vec::new(),
                parameter_registers: Vec::new(),
                successor_arguments: Vec::new(),
                nodes: (0..2).map(value).collect(),
                terminator: NumericTerminator::Return(value(1)),
            }],
            frame_states: vec![hir::NumericFrameState {
                point: NumericFramePoint::Node(value(1)),
                frames: Box::new([otter_vm::deopt::DeoptFrame {
                    function_id: 96,
                    byte_pc: 24,
                    entry: None,
                    slots: (vec![
                        hir::NumericFrameSlot::Value(value(0)),
                        hir::NumericFrameSlot::Undefined,
                    ])
                    .into(),
                }]),
            }],
            direct_call_targets: Vec::new(),
            operand_values: Vec::new(),
            parameter_count: 1,
            register_count: 2,
            arithmetic_op_count: 0,
        }
    }

    fn property_store_emission_view(value_is_non_cell: bool) -> JitCompileSnapshot {
        let (parameter_count, store_byte_pc, instructions) = if value_is_non_cell {
            (
                1,
                8,
                vec![
                    (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(7)]),
                    (
                        Op::StoreProperty,
                        vec![
                            Operand::Register(0),
                            Operand::ConstIndex(0),
                            Operand::Register(1),
                            Operand::Register(2),
                        ],
                    ),
                    (Op::ReturnValue, vec![Operand::Register(0)]),
                ],
            )
        } else {
            (
                2,
                0,
                vec![
                    (
                        Op::StoreProperty,
                        vec![
                            Operand::Register(0),
                            Operand::ConstIndex(0),
                            Operand::Register(1),
                            Operand::Register(2),
                        ],
                    ),
                    (Op::ReturnValue, vec![Operand::Register(0)]),
                ],
            )
        };
        let mut view = numeric_view(parameter_count, 3, instructions);
        view.cage_base = 0x1000;
        // Frozen object-body offsets asserted by the shared slab-base emitter.
        view.object_slab_handle_byte = 24;
        view.object_inline_values_byte = 64;
        view.property_programs.insert(
            store_byte_pc,
            vec![JitCacheIrProgram {
                ops: vec![
                    JitCacheIrOp::GuardShape {
                        object: 0,
                        shape: 7,
                    },
                    JitCacheIrOp::StoreField {
                        object: 0,
                        value_byte: 16,
                    },
                ]
                .into_boxed_slice(),
            }],
        );
        view
    }

    #[test]
    fn constructor_fields_select_the_owned_source_program() {
        let mut hir = property_selection_hir();
        let site = hir::NumericValue(3);
        hir.nodes[site.0] = NumericNode::ConstructorFieldStore {
            object: hir::NumericValue(0),
            value: hir::NumericValue(1),
            byte_pc: 40,
        };
        let transition = otter_vm::jit::JitConstructorFieldTransition {
            from_shape: 7,
            to_shape: 11,
            prototype_shapes: vec![13],
            slot: 0,
        };
        hir.constructor_field_sites
            .insert(site, (94, transition.clone()));
        let sequence = select(&hir).expect("source-owned field program");
        hir.constructor_field_sites
            .get_mut(&site)
            .unwrap()
            .1
            .to_shape = 17;
        assert!(sequence.instructions().iter().any(|instruction| matches!(
            instruction.opcode,
            MachineOpcode::CacheIrGuardShape {
                byte_pc: 40,
                shape: 7
            }
        )));
        assert!(sequence.instructions().iter().any(|instruction| matches!(
            instruction.opcode,
            MachineOpcode::CacheIrPublishShape {
                byte_pc: 40,
                shape: 11,
                ..
            }
        )));
        hir.constructor_field_sites.get_mut(&site).unwrap().0 = 95;
        assert!(
            select(&hir).is_err(),
            "a foreign source cannot supply the field program"
        );
    }

    #[test]
    fn selects_one_call_boundary_for_a_four_target_method_chain() {
        let sequence = select(&method_call_selection_hir()).expect("polymorphic method Machine IR");
        let calls = sequence
            .instructions()
            .iter()
            .filter(|instruction| matches!(instruction.opcode, MachineOpcode::Call(_)))
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        let call = calls[0];
        let MachineOpcode::Call(descriptor_index) = call.opcode else {
            unreachable!("filtered call")
        };
        let descriptor = &sequence.call_descriptors()[descriptor_index as usize];
        let CallTarget::Direct {
            kind, candidates, ..
        } = &descriptor.target
        else {
            panic!("direct method target")
        };
        assert_eq!(*kind, DirectCallKind::Method);
        assert_eq!(candidates.len(), 4);
        for (index, candidate) in candidates.iter().enumerate() {
            assert_eq!(candidate.target_index, index as u32);
            assert_eq!(candidate.target_count, 4);
            assert!(candidate.guard.is_some());
        }
        assert_eq!(call.safepoint, Some(SafepointId(0)));
        assert_eq!(call.deopt_id(), Some(DeoptId(0)));
        assert_eq!(
            call.operands
                .iter()
                .filter(|operand| operand.purpose == OperandPurpose::TaggedRoot)
                .count(),
            2,
            "receiver and argument roots must not be multiplied by candidate count"
        );
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| instruction.opcode == MachineOpcode::BoxInt32)
                .count(),
            1,
            "receiver boxing is shared by the whole chain"
        );
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| instruction.opcode == MachineOpcode::BoxBoolean)
                .count(),
            1,
            "argument boxing is shared by the whole chain"
        );
    }

    #[test]
    fn selects_wide_generic_methods_with_deduplicated_alias_roots_and_raw_packet() {
        for argument_count in [5_u32, 8, 300] {
            let mut sequence = select(&generic_wide_method_call_selection_hir(argument_count))
                .expect("wide generic method Machine IR");
            let call = sequence
                .instructions()
                .iter()
                .find(|instruction| matches!(instruction.opcode, MachineOpcode::Call(_)))
                .expect("wide method call");
            let MachineOpcode::Call(descriptor_index) = call.opcode else {
                unreachable!("selected call")
            };
            let descriptor = &sequence.call_descriptors()[descriptor_index as usize];
            let CallTarget::Direct {
                kind,
                argument_mode,
                candidates,
                ..
            } = &descriptor.target
            else {
                panic!("generic direct method target")
            };
            assert_eq!(*kind, DirectCallKind::Method);
            assert_eq!(*argument_mode, DirectCallArgumentMode::Fixed);
            assert!(candidates.is_empty());
            assert_eq!(descriptor.arguments.len(), argument_count as usize + 1);

            let roots = call
                .operands
                .iter()
                .filter(|operand| operand.purpose == OperandPurpose::TaggedRoot)
                .map(|operand| operand.value)
                .collect::<Vec<_>>();
            assert_eq!(roots, [MachineValue(0)]);
            assert_eq!(roots.iter().copied().collect::<BTreeSet<_>>().len(), 1);

            if argument_count == 8 {
                let allocation = sequence
                    .allocate(&TargetSpec::aarch64())
                    .expect("wide generic method allocation");
                let safepoints = lower_safepoints(&sequence, &allocation)
                    .expect("deduplicated method safepoints");
                let call_id = sequence
                    .instructions()
                    .iter()
                    .position(|instruction| matches!(instruction.opcode, MachineOpcode::Call(_)))
                    .map(|index| MachineInstructionId(index as u32))
                    .expect("wide method call identity");
                assert_eq!(
                    safepoints
                        .site(call_id)
                        .expect("call safepoint")
                        .roots
                        .len(),
                    1
                );
            }

            sequence.packed_double_view_cache_count = 3;
            let packet = value_packet_frame(&sequence).expect("method packet frame");
            assert_eq!(packet.raw_start, 6);
            assert_eq!(packet.raw_words, argument_count as u16 + 1);
        }
    }

    #[test]
    fn array_construct_selection_uses_typed_allocating_abi_and_exact_state() {
        let target = TargetSpec::aarch64();
        let sequence = select(&array_construct_selection_hir()).expect("ArrayConstruct Machine IR");
        assert_eq!(sequence.call_descriptors().len(), 1);
        let descriptor = &sequence.call_descriptors()[0];
        assert_eq!(
            descriptor.target,
            CallTarget::RuntimeStub(STUB_ARRAY_CONSTRUCT_ALLOC)
        );
        assert_eq!(descriptor.arguments, [MachineRepresentation::Tagged; 3]);
        assert_eq!(descriptor.results, [MachineRepresentation::Tagged]);
        assert_eq!(
            descriptor.effects,
            CallEffects::READS_HEAP.union(CallEffects::WRITES_HEAP)
        );
        assert_eq!(descriptor.exceptional, ExceptionalEdge::None);
        assert_eq!(descriptor.safepoint, SafepointKind::Gc);
        assert_eq!(
            descriptor.clobbers,
            target
                .clobbers(TargetClobberSet::ScalarCall)
                .to_vec()
                .into_iter()
                .filter(|register| *register != target.integer_result())
                .collect::<Vec<_>>()
        );

        let (call_id, call) = sequence
            .instructions()
            .iter()
            .enumerate()
            .find(|(_, instruction)| matches!(instruction.opcode, MachineOpcode::Call(0)))
            .map(|(index, instruction)| (MachineInstructionId(index as u32), instruction))
            .expect("typed allocating call");
        let expected_registers = [
            target.integer_argument(2).expect("argument 2"),
            target.integer_argument(3).expect("argument 3"),
            target.integer_argument(4).expect("argument 4"),
            target.integer_result(),
        ];
        for (operand, register) in call.operands[..4].iter().zip(expected_registers) {
            assert_eq!(operand.constraint, OperandConstraint::Fixed(register));
        }
        assert_ne!(
            call.operands[1].value, call.operands[2].value,
            "the two ABI padding values must remain separate SSA definitions"
        );
        assert_eq!(call.safepoint, Some(SafepointId(0)));
        assert!(call.deopt_id().is_some());
        assert!(sequence.instructions().iter().any(|instruction| {
            instruction.opcode == MachineOpcode::BoxInt32
                && instruction.operands[1].value == call.operands[0].value
                && instruction.operands[1].constraint
                    == OperandConstraint::Fixed(target.integer_argument(2).expect("argument 2"))
        }));
        for padding in [&call.operands[1], &call.operands[2]] {
            assert!(sequence.instructions().iter().any(|instruction| {
                instruction.opcode
                    == MachineOpcode::TaggedConstant(otter_vm::Value::undefined().to_bits())
                    && instruction.operands[0].value == padding.value
            }));
        }

        let allocation = sequence
            .allocate(&target)
            .expect("ArrayConstruct allocation");
        let locations = allocation
            .instruction_locations(call_id)
            .expect("ArrayConstruct call locations");
        for (&location, register) in locations[..4].iter().zip(expected_registers) {
            assert_eq!(location, AllocatedLocation::Register(register));
        }
    }

    #[test]
    fn derived_this_binding_splits_fast_cold_and_exception_edges_before_allocation() {
        for local_catch in [false, true] {
            let mut hir = committed_value_selection_hir(local_catch);
            let NumericNode::CommittedValue {
                operation, inputs, ..
            } = &mut hir.nodes[3]
            else {
                unreachable!()
            };
            *operation =
                CommittedValueOperation::Scalar(otter_vm::native_abi::ScalarValueOp::BindThisValue);
            inputs[1] = None;
            let sequence = select(&hir).expect("explicit derived-this Machine CFG");
            let (probe_block, probe) = sequence
                .blocks()
                .iter()
                .enumerate()
                .find_map(|(index, block)| {
                    sequence.instructions()[block.first.0 as usize..block.end.0 as usize]
                        .iter()
                        .find(|instruction| {
                            matches!(instruction.opcode, MachineOpcode::TryBindDerivedThis { .. })
                        })
                        .map(|probe| (index, probe))
                })
                .expect("generated binding probe");
            assert!(probe.safepoint.is_none() && probe.exits.is_empty());
            let cold = sequence.blocks()[probe_block].successors[1].0 as usize;
            assert_eq!(
                super::super::derived_this::cold_byte_pc(&sequence, cold),
                Some(48)
            );
            let cold_instruction =
                &sequence.instructions()[sequence.blocks()[cold].first.0 as usize];
            assert!(cold_instruction.safepoint.is_some());
            assert!(
                cold_instruction
                    .operands
                    .iter()
                    .any(|operand| operand.purpose == OperandPurpose::TaggedRoot)
            );
            assert_eq!(sequence.blocks()[cold].successors.len(), 3);
            assert_eq!(
                sequence.instructions()[sequence.blocks()[cold].end.0 as usize - 1].opcode,
                MachineOpcode::BranchNativeStatus
            );
            let MachineOpcode::Call(descriptor) = cold_instruction.opcode else {
                unreachable!()
            };
            assert!(is_explicit_committed_runtime_call(
                &sequence.call_descriptors()[descriptor as usize]
            ));
            let allocation = sequence
                .allocate(&TargetSpec::aarch64())
                .expect("all SSA critical edges are split");
            assert!(allocation.used_register_count() > 0);
            lower_safepoints(&sequence, &allocation)
                .expect("rebuilt safepoint order and allocated roots agree");
            let mut malformed = sequence.clone();
            let branch = malformed.blocks[probe_block].end.0 as usize - 1;
            malformed.instructions[branch].opcode = MachineOpcode::BranchIf(false);
            assert!(
                malformed.verify(&TargetSpec::aarch64()).is_err(),
                "a successful bind must bypass the cold call"
            );
        }
    }

    #[test]
    fn committed_value_selection_has_pure_landing_and_complete_tagged_roots() {
        for local_catch in [false, true] {
            let sequence = select(&committed_value_selection_hir(local_catch))
                .expect("committed value Machine IR");
            let descriptor = sequence
                .call_descriptors()
                .iter()
                .find(|descriptor| matches!(descriptor.target, CallTarget::CommittedRuntime { .. }))
                .expect("committed descriptor");
            assert_eq!(
                descriptor.target,
                CallTarget::CommittedRuntime {
                    target: otter_vm::native_abi::STUB_JIT_SCALAR_VALUE,
                    logical_pc: 6,
                    byte_pc: 48,
                    semantic_arity: 2,
                }
            );
            if local_catch {
                assert!(matches!(
                    descriptor.exceptional,
                    ExceptionalEdge::LandingPad(_)
                ));
                let acknowledgement_descriptor = sequence
                    .call_descriptors()
                    .iter()
                    .position(|candidate| {
                        candidate.target
                            == CallTarget::RuntimeStub(STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW)
                    })
                    .expect("caught-throw acknowledgement descriptor");
                let acknowledgements = sequence
                    .instructions()
                    .iter()
                    .enumerate()
                    .filter_map(|(index, instruction)| {
                        matches!(
                            instruction.opcode,
                            MachineOpcode::Call(descriptor)
                                if descriptor as usize == acknowledgement_descriptor
                        )
                        .then_some(MachineInstructionId(index as u32))
                    })
                    .collect::<Vec<_>>();
                assert_eq!(acknowledgements.len(), 1);
                let acknowledgement = acknowledgements[0];
                let acknowledgement_block = sequence
                    .blocks()
                    .iter()
                    .find(|block| {
                        block.first.0 <= acknowledgement.0 && acknowledgement.0 < block.end.0
                    })
                    .expect("dedicated acknowledgement block");
                assert_eq!(acknowledgement_block.first, acknowledgement);
                assert_eq!(acknowledgement_block.predecessors.len(), 1);
                assert_eq!(acknowledgement_block.successors.len(), 1);
            } else {
                assert_eq!(descriptor.exceptional, ExceptionalEdge::Propagate);
            }
            let call = sequence
                .instructions()
                .iter()
                .find(|instruction| {
                    matches!(instruction.opcode, MachineOpcode::Call(index)
                        if sequence.call_descriptors()[index as usize] == *descriptor)
                })
                .expect("committed call");
            assert_eq!(call.deopt_id(), None);
            assert_eq!(call.safepoint, Some(SafepointId(0)));
            let roots = call
                .operands
                .iter()
                .filter(|operand| operand.purpose == OperandPurpose::TaggedRoot)
                .map(|operand| operand.value)
                .collect::<BTreeSet<_>>();
            assert_eq!(
                roots,
                BTreeSet::from([MachineValue(0), MachineValue(1), MachineValue(2)]),
                "the unrelated live tagged value is rooted with both semantic inputs"
            );
            sequence
                .verify(&TargetSpec::aarch64())
                .expect("committed sequence verification");
        }
    }

    #[test]
    fn cold_call_selection_is_deopt_only_and_allocates_no_call_state() {
        let sequence = select(&cold_call_selection_hir()).expect("cold call Machine IR");
        let (call_id, call) = sequence
            .instructions()
            .iter()
            .enumerate()
            .find(|(_, instruction)| matches!(instruction.opcode, MachineOpcode::Call(_)))
            .expect("cold call instruction");
        let MachineOpcode::Call(descriptor_index) = call.opcode else {
            unreachable!("selected call")
        };
        let descriptor = &sequence.call_descriptors()[descriptor_index as usize];
        assert_eq!(
            descriptor.target,
            CallTarget::ColdCallExit {
                kind: ColdCallKind::Plain,
                caller_function_id: 151,
                logical_pc: 5,
                byte_pc: 40,
            }
        );
        assert_eq!(descriptor.arguments, Vec::new());
        assert_eq!(descriptor.results, [MachineRepresentation::Tagged]);
        assert_eq!(descriptor.effects, CallEffects::PURE);
        assert_eq!(descriptor.safepoint, SafepointKind::None);
        assert!(descriptor.clobbers.is_empty());
        assert!(call.clobbers.is_empty());
        assert_eq!(call.safepoint, None);
        assert_eq!(call.deopt_id(), Some(DeoptId(0)));
        assert!(call.operands.iter().all(|operand| {
            !matches!(
                operand.purpose,
                OperandPurpose::Input | OperandPurpose::TaggedRoot | OperandPurpose::CellRoot
            )
        }));
        assert!(sequence.instructions().iter().all(|instruction| {
            !matches!(
                instruction.opcode,
                MachineOpcode::BoxInt32
                    | MachineOpcode::BoxUint32
                    | MachineOpcode::BoxNumber
                    | MachineOpcode::BoxBoolean
            )
        }));
        assert_eq!(
            sequence.verify(&TargetSpec::aarch64()),
            Ok(()),
            "cold call i{call_id} retains a valid exact deopt contract"
        );
    }

    #[test]
    fn verifier_rejects_partial_or_malformed_direct_method_chains() {
        let valid = select(&method_call_selection_hir()).expect("valid method chain");
        let assert_invalid =
            |mut sequence: InstructionSequence,
             mutate: &dyn Fn(&mut DirectCallKind, &mut Vec<DirectCallCandidate>)| {
                let (call_id, descriptor_index) = sequence
                    .instructions
                    .iter()
                    .enumerate()
                    .find_map(|(index, instruction)| match instruction.opcode {
                        MachineOpcode::Call(descriptor_index) => Some((index, descriptor_index)),
                        _ => None,
                    })
                    .expect("direct call");
                let CallTarget::Direct {
                    kind, candidates, ..
                } = &mut sequence.call_descriptors[descriptor_index as usize].target
                else {
                    panic!("direct target")
                };
                mutate(kind, candidates);
                assert_eq!(
                    sequence.verify(&TargetSpec::aarch64()),
                    Err(crate::machine::VerificationError::InvalidCallTarget(
                        MachineInstructionId(call_id as u32)
                    ))
                );
            };

        let mut generic = valid.clone();
        let CallTarget::Direct { candidates, .. } = &mut generic.call_descriptors[0].target else {
            panic!("direct method target")
        };
        candidates.clear();
        assert_eq!(generic.verify(&TargetSpec::aarch64()), Ok(()));

        assert_invalid(valid.clone(), &|kind, candidates| {
            candidates.clear();
            *kind = DirectCallKind::Plain;
        });
        assert_invalid(valid.clone(), &|_, candidates| {
            candidates[1].target_index = 2;
        });
        assert_invalid(valid.clone(), &|_, candidates| {
            candidates[1].target_count = 3;
        });
        assert_invalid(valid.clone(), &|_, candidates| {
            candidates[1].guard = None;
        });
        assert_invalid(valid.clone(), &|_, candidates| {
            candidates[1]
                .guard
                .as_mut()
                .expect("method guard")
                .method_fid += 1;
        });
        assert_invalid(valid.clone(), &|_, candidates| {
            let mut fifth = candidates[3].clone();
            fifth.target_index = 4;
            fifth.target_count = 5;
            fifth.guard.as_mut().expect("method guard").method_fid = fifth.callee.plan.function_id;
            for candidate in candidates.iter_mut() {
                candidate.target_count = 5;
            }
            candidates.push(fifth);
        });
        assert_invalid(valid, &|kind, candidates| {
            *kind = DirectCallKind::Plain;
            for candidate in candidates {
                candidate.guard = None;
            }
        });
    }

    #[test]
    fn verifier_requires_the_complete_cold_exit_contract() {
        let valid = select(&cold_call_selection_hir()).expect("valid cold call");
        let call_id = valid
            .instructions
            .iter()
            .position(|instruction| matches!(instruction.opcode, MachineOpcode::Call(_)))
            .expect("cold call");

        let mut missing_deopt = valid.clone();
        missing_deopt.instructions[call_id].exits = Box::default();
        assert_eq!(
            missing_deopt.verify(&TargetSpec::aarch64()),
            Err(crate::machine::VerificationError::InvalidCallTarget(
                MachineInstructionId(call_id as u32)
            ))
        );

        let mut effectful = valid;
        let MachineOpcode::Call(descriptor_index) = effectful.instructions[call_id].opcode else {
            unreachable!("cold call")
        };
        effectful.call_descriptors[descriptor_index as usize].effects = CallEffects::READS_HEAP;
        assert_eq!(
            effectful.verify(&TargetSpec::aarch64()),
            Err(crate::machine::VerificationError::InvalidCallTarget(
                MachineInstructionId(call_id as u32)
            ))
        );
    }

    #[test]
    fn selects_binding_reads_as_explicit_hit_cold_status_and_join_cfg() {
        let sequence = select(&binding_selection_hir()).expect("binding Machine CFG");

        let guards = sequence
            .instructions()
            .iter()
            .filter(|instruction| matches!(instruction.opcode, MachineOpcode::BindingGuard { .. }))
            .collect::<Vec<_>>();
        assert_eq!(guards.len(), 2);
        assert!(
            guards
                .iter()
                .all(|guard| guard.exits.is_empty() && guard.safepoint.is_none())
        );
        assert!(guards.iter().any(|guard| matches!(
            guard.opcode,
            MachineOpcode::BindingGuard {
                byte_pc: 24,
                semantics: BindingSemantics::Read(BindingRead::Global { .. }),
                target: MachineBindingTarget::Global(BindingHitProof::GlobalLexical {
                    cell_offset: 0x1234,
                    writable: true,
                }),
            }
        )));
        assert!(guards.iter().any(|guard| matches!(
            guard.opcode,
            MachineOpcode::BindingGuard {
                byte_pc: 32,
                semantics: BindingSemantics::Read(BindingRead::Global { .. }),
                target: MachineBindingTarget::Global(BindingHitProof::GlobalObject {
                    shape: 0x5678,
                    dictionary: true,
                    value_byte: 40,
                    global_lexical_epoch: 9,
                    writable: true,
                }),
            }
        )));

        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| matches!(
                    instruction.opcode,
                    MachineOpcode::BindingHit { .. }
                ))
                .count(),
            2
        );
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| matches!(
                    instruction.opcode,
                    MachineOpcode::BindingJoin { .. }
                ))
                .count(),
            2
        );
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| instruction.opcode == MachineOpcode::BranchNativeStatus)
                .count(),
            2
        );

        let committed_calls = sequence
            .instructions()
            .iter()
            .filter_map(|instruction| {
                let MachineOpcode::Call(descriptor) = instruction.opcode else {
                    return None;
                };
                let descriptor = &sequence.call_descriptors[descriptor as usize];
                is_explicit_committed_runtime_call(descriptor).then_some((instruction, descriptor))
            })
            .collect::<Vec<_>>();
        assert_eq!(committed_calls.len(), 2);
        for (call, descriptor) in committed_calls {
            assert!(call.safepoint.is_some());
            assert_eq!(call.deopt_id(), None);
            assert_eq!(
                descriptor.results,
                [
                    MachineRepresentation::Tagged,
                    MachineRepresentation::NativeStatus,
                ]
            );
            assert_eq!(descriptor.exceptional, ExceptionalEdge::None);
        }

        let normalized = sequence.normalized();
        assert!(normalized.contains("BindingGuard { byte_pc: 24"));
        assert!(normalized.contains("BindingHit { byte_pc: 32"));
        assert!(normalized.contains("BranchNativeStatus"));
        sequence
            .allocate(&TargetSpec::aarch64())
            .expect("binding CFG allocation");
    }

    #[test]
    fn generated_binding_writes_keep_direct_stores_and_explicit_barriers() {
        let sequence = select(&binding_write_selection_hir()).expect("binding-write Machine CFG");
        let write_hits = sequence
            .instructions()
            .iter()
            .filter(|instruction| {
                matches!(
                    instruction.opcode,
                    MachineOpcode::BindingHit {
                        semantics: BindingSemantics::Write(_),
                        ..
                    }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(write_hits.len(), 3);
        assert!(
            write_hits
                .iter()
                .all(|instruction| instruction.exits.is_empty() && instruction.safepoint.is_none())
        );
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| instruction.opcode == MachineOpcode::BindingWriteBarrier)
                .count(),
            write_hits.len()
        );
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| {
                    let MachineOpcode::Call(descriptor) = instruction.opcode else {
                        return false;
                    };
                    is_explicit_committed_runtime_call(
                        &sequence.call_descriptors[descriptor as usize],
                    )
                })
                .count(),
            write_hits.len()
        );
        sequence
            .allocate(&TargetSpec::aarch64())
            .expect("binding-write allocation");
    }

    #[test]
    fn binding_root_states_do_not_pollute_later_exact_deopt_metadata() {
        let hir = binding_followed_by_deopt_hir();
        assert!(hir.frame_states.iter().any(|state| matches!(
            state.point,
            NumericFramePoint::Node(node)
                if hir.nodes[node.0].frame_state_purpose()
                    == Some(NumericFrameStatePurpose::TaggedRoots)
        )));
        let sequence = select(&hir).expect("binding-to-deopt Machine CFG");
        let allocation = sequence
            .allocate(&TargetSpec::aarch64())
            .expect("binding-to-deopt allocation");
        let safepoints =
            lower_safepoints(&sequence, &allocation).expect("binding-to-deopt safepoints");
        let frame = arm64::frame_layout(&allocation, safepoints.root_slot_count())
            .expect("binding-to-deopt frame");
        let states = machine_frame_states(&hir);
        assert_eq!(states.len(), hir.frame_states.len());
        lower_deopt_table(
            &sequence,
            &allocation,
            frame,
            arm64::GPR_BUDGET,
            arm64::FP_BUDGET,
            &states,
        )
        .expect("only exact/runtime-metadata states require allocator deopt locations");
    }

    #[test]
    fn every_schema_binding_operation_uses_the_same_committed_machine_cfg() {
        let mut covered = 0;
        for schema in OPCODE_SCHEMA
            .iter()
            .filter(|schema| schema.binding.is_some())
        {
            covered += 1;
            let semantics = schema.binding.expect("filtered binding schema");
            let hir = schema_binding_hir(schema);
            assert!(hir.nodes.iter().any(|node| matches!(
                node,
                NumericNode::Binding {
                    semantics: copied,
                    target: None,
                    byte_pc: 24,
                    ..
                } if *copied == semantics
            )));

            let sequence =
                select(&hir).unwrap_or_else(|error| panic!("select {:?}: {error:?}", schema.op));
            assert_eq!(
                sequence
                    .instructions()
                    .iter()
                    .filter(|instruction| matches!(
                        instruction.opcode,
                        MachineOpcode::BindingGuard {
                            semantics: copied,
                            target: MachineBindingTarget::Cold,
                            byte_pc: 24,
                        } if copied == semantics
                    ))
                    .count(),
                1,
                "{:?}",
                schema.op
            );
            assert!(sequence.instructions().iter().all(|instruction| !matches!(
                instruction.opcode,
                MachineOpcode::BindingHit { .. }
            )));
            let calls = sequence
                .instructions()
                .iter()
                .filter(|instruction| {
                    let MachineOpcode::Call(descriptor) = instruction.opcode else {
                        return false;
                    };
                    is_explicit_committed_runtime_call(
                        &sequence.call_descriptors[descriptor as usize],
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(calls.len(), 1, "{:?}", schema.op);
            assert!(calls[0].safepoint.is_some(), "{:?}", schema.op);
            assert_eq!(calls[0].deopt_id(), None, "{:?}", schema.op);
            assert_eq!(
                sequence
                    .instructions()
                    .iter()
                    .filter(|instruction| instruction.opcode == MachineOpcode::BranchNativeStatus)
                    .count(),
                1,
                "{:?}",
                schema.op
            );
            sequence
                .allocate(&TargetSpec::aarch64())
                .unwrap_or_else(|error| panic!("allocate {:?}: {error:?}", schema.op));
        }
        assert!(covered > 0, "opcode schema must expose the binding family");
    }

    #[test]
    fn caught_binding_throw_routes_exception_ssa_through_one_acknowledged_edge() {
        let sequence = select(&binding_catch_hir()).expect("caught binding Machine CFG");
        let (call_id, payload) = sequence
            .instructions()
            .iter()
            .enumerate()
            .find_map(|(id, instruction)| {
                let MachineOpcode::Call(descriptor) = instruction.opcode else {
                    return None;
                };
                if !is_explicit_committed_runtime_call(
                    &sequence.call_descriptors[descriptor as usize],
                ) {
                    return None;
                }
                instruction
                    .operands
                    .iter()
                    .find(|operand| {
                        operand.purpose == OperandPurpose::Output
                            && sequence.representations[operand.value.0 as usize]
                                == MachineRepresentation::Tagged
                    })
                    .map(|operand| (id, operand.value))
            })
            .expect("committed binding pair");
        let cold = sequence
            .blocks()
            .iter()
            .position(|block| block.first.0 <= call_id as u32 && (call_id as u32) < block.end.0)
            .expect("binding cold block");
        let throw_edge = sequence.blocks()[cold].successors[1];
        let acknowledgement =
            &sequence.instructions()[sequence.blocks()[throw_edge.0 as usize].first.0 as usize];
        let MachineOpcode::Call(descriptor) = acknowledgement.opcode else {
            panic!("caught throw edge must begin with explicit acknowledgement")
        };
        assert!(matches!(
            sequence.call_descriptors[descriptor as usize].target,
            CallTarget::RuntimeStub(target)
                if target.id == otter_vm::native_abi::STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW.id
        ));
        assert!(
            sequence.blocks()[throw_edge.0 as usize]
                .successor_arguments
                .iter()
                .flatten()
                .any(|&argument| argument == payload),
            "{}",
            sequence.normalized()
        );
        assert!(
            sequence
                .instructions()
                .iter()
                .all(|instruction| instruction.opcode != MachineOpcode::Throw)
        );
        sequence
            .allocate(&TargetSpec::aarch64())
            .expect("caught binding allocation");
    }

    #[test]
    fn selects_prepared_string_as_a_pure_relocation_load() {
        let sequence = select(&string_constant_selection_hir()).expect("string-cell Machine IR");
        let load_id = sequence
            .instructions()
            .iter()
            .position(|instruction| {
                matches!(
                    instruction.opcode,
                    MachineOpcode::StringConstantCellLoad {
                        byte_pc: 24,
                        target: otter_vm::jit::JitStringConstantCell { cell_addr: 0x1238 }
                    }
                )
            })
            .expect("selected string-cell load");
        let load = &sequence.instructions()[load_id];
        assert_eq!(
            load.operands,
            [MachineOperand::register_output(MachineValue(0))]
        );
        assert_eq!(
            load.clobbers,
            string_constant_cell_load_clobbers(&TargetSpec::aarch64())
        );
        assert_eq!(load.deopt_id(), None);
        assert_eq!(load.safepoint, None);
        assert_eq!(sequence.representations()[0], MachineRepresentation::Tagged);
        assert!(sequence.normalized().contains("StringConstantCellLoad"));
        sequence
            .allocate(&TargetSpec::aarch64())
            .expect("string-cell allocation");

        let mut malformed = sequence;
        malformed.instructions[load_id].clobbers.clear();
        assert_eq!(
            malformed.verify(&TargetSpec::aarch64()),
            Err(crate::machine::VerificationError::OpcodeSignatureMismatch(
                MachineInstructionId(load_id as u32)
            ))
        );

        for invalid_addr in [0, 1] {
            let mut malformed =
                select(&string_constant_selection_hir()).expect("fresh string-cell Machine IR");
            let MachineOpcode::StringConstantCellLoad { byte_pc, .. } =
                malformed.instructions[load_id].opcode
            else {
                unreachable!("selected string-cell load")
            };
            malformed.instructions[load_id].opcode = MachineOpcode::StringConstantCellLoad {
                byte_pc,
                target: otter_vm::jit::JitStringConstantCell {
                    cell_addr: invalid_addr,
                },
            };
            assert_eq!(
                malformed.verify(&TargetSpec::aarch64()),
                Err(crate::machine::VerificationError::OpcodeSignatureMismatch(
                    MachineInstructionId(load_id as u32)
                )),
                "invalid cell address {invalid_addr:#x}"
            );
        }
    }

    #[test]
    fn verifier_rejects_binding_status_without_the_committed_pair_producer() {
        let mut malformed = select(&binding_selection_hir()).expect("valid binding CFG");
        let branch_id = malformed
            .instructions
            .iter()
            .position(|instruction| instruction.opcode == MachineOpcode::BranchNativeStatus)
            .expect("explicit native-status branch");
        let expected = Err(crate::machine::VerificationError::OpcodeSignatureMismatch(
            MachineInstructionId(branch_id as u32),
        ));

        let unowned_status = MachineValue(malformed.representations.len() as u32);
        malformed
            .representations
            .push(MachineRepresentation::NativeStatus);
        malformed.instructions[branch_id].operands[0] =
            MachineOperand::register_input(unowned_status);
        assert_eq!(malformed.verify(&TargetSpec::aarch64()), expected);
    }

    #[test]
    fn verifier_confines_binding_raw_addresses_to_the_generated_hit_block() {
        let valid = select(&binding_selection_hir()).expect("valid binding CFG");
        let guard_id = valid
            .instructions
            .iter()
            .position(|instruction| {
                matches!(instruction.opcode, MachineOpcode::BindingGuard { .. })
            })
            .expect("binding guard");
        let owner = valid.instructions[guard_id].operands[1].value;
        let expected = Err(crate::machine::VerificationError::OpcodeSignatureMismatch(
            MachineInstructionId(guard_id as u32),
        ));

        let mut safepoint_escape = valid.clone();
        let cold_call = safepoint_escape
            .instructions
            .iter()
            .position(|instruction| {
                let MachineOpcode::Call(descriptor) = instruction.opcode else {
                    return false;
                };
                is_explicit_committed_runtime_call(
                    &safepoint_escape.call_descriptors[descriptor as usize],
                )
            })
            .expect("binding cold call");
        safepoint_escape.instructions[cold_call]
            .operands
            .push(MachineOperand::tagged_root(owner));
        assert_eq!(safepoint_escape.verify(&TargetSpec::aarch64()), expected);

        let mut edge_escape = valid;
        let guard_block = edge_escape
            .blocks
            .iter()
            .position(|block| block.first.0 <= guard_id as u32 && (guard_id as u32) < block.end.0)
            .expect("guard block");
        let hit = edge_escape.blocks[guard_block].successors[0];
        let join = edge_escape.blocks[hit.0 as usize].successors[0];
        let escaped_parameter = MachineValue(edge_escape.representations.len() as u32);
        edge_escape
            .representations
            .push(MachineRepresentation::Int64);
        edge_escape.blocks[hit.0 as usize].successor_arguments[0].push(owner);
        edge_escape.blocks[join.0 as usize]
            .parameters
            .push(escaped_parameter);
        assert_eq!(edge_escape.verify(&TargetSpec::aarch64()), expected);
    }

    #[test]
    fn selects_tagged_nullish_equality_with_exact_deopt_and_boxing() {
        for equal in [true, false] {
            let sequence = select(&tagged_nullish_selection_hir(equal))
                .expect("tagged nullish equality Machine IR");
            let compare = sequence
                .instructions()
                .iter()
                .find(|instruction| {
                    instruction.opcode == MachineOpcode::TaggedNullishEqual { byte_pc: 24, equal }
                })
                .expect("selected tagged nullish equality");
            assert_eq!(
                compare.operands,
                [
                    MachineOperand::register_input(MachineValue(0)),
                    MachineOperand::register_output(MachineValue(1)),
                    MachineOperand::frame_value(MachineValue(0)),
                ]
            );
            assert_eq!(
                sequence.representations()[compare.operands[0].value.0 as usize],
                MachineRepresentation::Tagged
            );
            assert_eq!(
                sequence.representations()[compare.operands[1].value.0 as usize],
                MachineRepresentation::Boolean
            );
            assert_eq!(
                compare.clobbers,
                TargetSpec::aarch64().clobbers(TargetClobberSet::StatusScratch)
            );
            assert_eq!(compare.deopt_id(), Some(DeoptId(0)));
            assert_eq!(compare.safepoint, None);
            assert!(sequence.instructions().iter().any(|instruction| {
                instruction.opcode == MachineOpcode::BoxBoolean
                    && instruction.operands
                        == [
                            MachineOperand::register_input(MachineValue(1)),
                            MachineOperand::register_output(MachineValue(2)),
                        ]
            }));
            assert!(sequence.normalized().contains(&format!(
                "TaggedNullishEqual {{ byte_pc: 24, equal: {equal} }}"
            )));
            sequence
                .allocate(&TargetSpec::aarch64())
                .expect("tagged nullish equality allocation");
        }
    }

    #[test]
    fn verifier_rejects_malformed_tagged_nullish_equality() {
        let valid = select(&tagged_nullish_selection_hir(true)).expect("valid nullish equality");
        let compare_id = valid
            .instructions
            .iter()
            .position(|instruction| {
                matches!(instruction.opcode, MachineOpcode::TaggedNullishEqual { .. })
            })
            .expect("tagged nullish equality");
        let expected = Err(crate::machine::VerificationError::OpcodeSignatureMismatch(
            MachineInstructionId(compare_id as u32),
        ));

        let mut missing_input = valid.clone();
        missing_input.instructions[compare_id].operands.remove(0);
        assert_eq!(missing_input.verify(&TargetSpec::aarch64()), expected);

        let mut wrong_input_representation = valid.clone();
        wrong_input_representation.representations[0] = MachineRepresentation::Int32;
        assert_eq!(
            wrong_input_representation.verify(&TargetSpec::aarch64()),
            expected
        );

        let mut spilled_input = valid.clone();
        spilled_input.instructions[compare_id].operands[0] =
            MachineOperand::location_input(MachineValue(0));
        assert_eq!(spilled_input.verify(&TargetSpec::aarch64()), expected);

        let mut wrong_output_representation = valid.clone();
        wrong_output_representation.representations[1] = MachineRepresentation::Tagged;
        assert_eq!(
            wrong_output_representation.verify(&TargetSpec::aarch64()),
            expected
        );

        let mut output_in_preop_state = valid.clone();
        output_in_preop_state.instructions[compare_id]
            .operands
            .push(MachineOperand::frame_value(MachineValue(1)));
        assert_eq!(
            output_in_preop_state.verify(&TargetSpec::aarch64()),
            expected
        );

        let mut duplicate_deopt = valid.clone();
        duplicate_deopt.instructions[compare_id]
            .operands
            .push(MachineOperand::frame_value(MachineValue(0)));
        assert_eq!(duplicate_deopt.verify(&TargetSpec::aarch64()), expected);

        let mut missing_deopt = valid.clone();
        missing_deopt.instructions[compare_id].exits = Box::default();
        assert_eq!(missing_deopt.verify(&TargetSpec::aarch64()), expected);

        let mut spurious_safepoint = valid.clone();
        spurious_safepoint.instructions[compare_id].safepoint = Some(SafepointId(9));
        assert_eq!(spurious_safepoint.verify(&TargetSpec::aarch64()), expected);

        let mut wrong_clobber = valid;
        wrong_clobber.instructions[compare_id].clobbers.clear();
        assert_eq!(wrong_clobber.verify(&TargetSpec::aarch64()), expected);
    }

    #[test]
    fn selects_properties_with_explicit_completion_and_cold_roots() {
        let hir = property_selection_hir();
        let sequence = select(&hir).expect("property Machine IR");
        let sources = sequence
            .instructions()
            .iter()
            .filter(|instruction| {
                matches!(instruction.opcode, MachineOpcode::PropertySource { .. })
            })
            .collect::<Vec<_>>();
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().all(|source| source.operands.len() == 1));
        let load_source = sources
            .iter()
            .find(|source| {
                matches!(
                    source.opcode,
                    MachineOpcode::PropertySource { store: false, .. }
                )
            })
            .unwrap();
        let load_cell = load_source.operands[0].value;
        let cold = sequence.instructions().iter().find(|instruction| match instruction.opcode {
            MachineOpcode::Call(index) => matches!(sequence.call_descriptors()[index as usize].target, CallTarget::CommittedRuntime { target, .. } if target == otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY),
            _ => false,
        }).expect("explicit named-load cold call");
        assert!(cold.safepoint.is_some());
        assert!(cold.exits.is_empty());
        assert_eq!(cold.operands[1], MachineOperand::location_input(load_cell));
        let store_source = sources
            .iter()
            .find(|source| {
                matches!(
                    source.opcode,
                    MachineOpcode::PropertySource { store: true, .. }
                )
            })
            .unwrap();
        let store_cell = store_source.operands[0].value;
        let cold = sequence.instructions().iter().find(|instruction| match instruction.opcode {
            MachineOpcode::Call(index) => matches!(sequence.call_descriptors()[index as usize].target,
                CallTarget::CommittedRuntime { target, .. } if target == otter_vm::native_abi::STUB_JIT_STORE_PROPERTY),
            _ => false,
        }).expect("explicit named-store cold call");
        assert!(cold.safepoint.is_some() && cold.exits.is_empty());
        assert_eq!(cold.operands[2], MachineOperand::location_input(store_cell));
        let normalized = sequence.normalized();
        assert!(normalized.contains("PropertySource"));
        assert!(normalized.contains("CacheIrJoin"));
        assert!(!normalized.contains("MachinePropertySite"));
        sequence
            .allocate(&TargetSpec::aarch64())
            .expect("property late-location allocation");
    }

    #[test]
    fn property_store_barrier_classification_uses_the_pre_boxing_scalar_type() {
        for value_type in [
            NumericType::Int32,
            NumericType::Uint32,
            NumericType::Number,
            NumericType::Boolean,
        ] {
            assert!(property_store_value_is_non_cell(value_type));
        }
        assert!(!property_store_value_is_non_cell(NumericType::Tagged));

        let mut hir = property_selection_hir();
        hir.nodes[1] = NumericNode::TaggedConstant(Value::undefined().to_bits());
        let sequence = select(&hir).expect("tagged property-store Machine IR");
        let store = sequence
            .instructions()
            .iter()
            .find(|instruction| {
                matches!(
                    instruction.opcode,
                    MachineOpcode::CacheIrWriteBarrier {
                        value_is_non_cell: false,
                        ..
                    }
                )
            })
            .expect("tagged property store");
        assert_eq!(
            store.clobbers,
            TargetSpec::aarch64()
                .clobbers(TargetClobberSet::ScalarCall)
                .to_vec()
        );
    }

    #[test]
    fn property_transition_selects_guards_store_publication_and_barriers_in_order() {
        let sequence = select(&property_transition_selection_hir())
            .expect("add-transition CacheIR Machine IR");
        let opcodes = sequence
            .instructions()
            .iter()
            .map(|instruction| &instruction.opcode)
            .collect::<Vec<_>>();
        let position = |predicate: fn(&MachineOpcode) -> bool| {
            opcodes
                .iter()
                .position(|opcode| predicate(opcode))
                .expect("transition opcode")
        };
        let guard_shape =
            position(|opcode| matches!(opcode, MachineOpcode::CacheIrGuardShape { .. }));
        let guard_prototype =
            position(|opcode| matches!(opcode, MachineOpcode::CacheIrGuardPrototypeNull { .. }));
        let guard_extensible =
            position(|opcode| matches!(opcode, MachineOpcode::CacheIrGuardExtensible { .. }));
        let store = position(|opcode| matches!(opcode, MachineOpcode::CacheIrStoreField { .. }));
        let publish =
            position(|opcode| matches!(opcode, MachineOpcode::CacheIrPublishShape { .. }));
        assert!(guard_shape < guard_prototype);
        assert!(guard_prototype < guard_extensible);
        assert!(guard_extensible < store);
        assert!(store < publish);
        assert_eq!(
            opcodes
                .iter()
                .filter(|opcode| matches!(opcode, MachineOpcode::CacheIrWriteBarrier { .. }))
                .count(),
            2,
            "value and child-shape edges each need an explicit barrier"
        );
        sequence
            .allocate(&TargetSpec::aarch64())
            .expect("transition allocation");
    }

    #[test]
    fn property_store_emission_keeps_shape_barrier_and_classifies_value_barrier() {
        let compile_relocations = |value_is_non_cell, function_name: &str| {
            let output = compile_output(
                &property_store_emission_view(value_is_non_cell),
                Some(ArtifactRequest {
                    identity: JitArtifactIdentity {
                        function_name: function_name.to_owned(),
                        module: "test:machine-property-store-barrier".to_owned(),
                    },
                    tier: JitDebugTier::Optimizing,
                    entry: JitDebugTarget::Entry,
                }),
            );
            String::from_utf8(
                output
                    .artifact
                    .expect("property-store artifact")
                    .file(JitArtifactFileName::Relocations)
                    .expect("property-store relocations")
                    .contents()
                    .to_vec(),
            )
            .expect("property-store relocations are UTF-8")
        };

        let non_cell = compile_relocations(true, "storeInt32");
        assert_eq!(
            non_cell.matches("\"name\": \"write_barrier\"").count(),
            0,
            "a proven non-cell existing-field store needs no barrier: \
             {non_cell}"
        );

        let tagged = compile_relocations(false, "storeTagged");
        assert_eq!(
            tagged.matches("\"name\": \"write_barrier\"").count(),
            1,
            "a tagged existing-field store emits one conditional value barrier: {tagged}"
        );
    }

    #[test]
    fn property_load_selects_explicit_cold_status_cfg() {
        let view = numeric_view(
            1,
            2,
            vec![
                (
                    Op::LoadProperty,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ],
        );
        let hir = NumericFunction::build(&view).unwrap();
        let sequence = select(&hir).expect("explicit property CFG");
        assert!(
            sequence
                .instructions()
                .iter()
                .any(|instruction| matches!(instruction.opcode, MachineOpcode::BranchNativeStatus))
        );
        let allocation = sequence
            .allocate(&TargetSpec::aarch64())
            .expect("property CFG allocation");
        assert!(!allocation.normalized().is_empty());
    }

    #[test]
    fn property_selection_rejects_a_program_from_a_different_activation() {
        let mut hir = property_selection_hir();
        hir.property_sites
            .get_mut(&hir::NumericValue(2))
            .unwrap()
            .function_id += 1;
        assert!(
            select(&hir).is_err(),
            "foreign property facts must not enter Machine IR"
        );
    }

    #[test]
    fn property_programs_survive_equal_source_ids_in_distinct_snapshots() {
        let mut first = property_store_emission_view(true);
        let mut second = first.clone();
        for (view, shape, value_byte) in [(&mut first, 101, 8), (&mut second, 202, 16)] {
            view.property_programs.insert(
                8,
                vec![JitCacheIrProgram {
                    ops: vec![
                        JitCacheIrOp::GuardShape { object: 0, shape },
                        JitCacheIrOp::StoreField {
                            object: 0,
                            value_byte,
                        },
                    ]
                    .into_boxed_slice(),
                }],
            );
        }
        let first_hir = NumericFunction::build(&first).unwrap();
        let second_hir = NumericFunction::build(&second).unwrap();
        for (hir, expected_shape, expected_slot) in [(&first_hir, 101, 8), (&second_hir, 202, 16)] {
            let sequence = select(hir).unwrap();
            assert!(sequence.instructions().iter().any(|instruction| matches!(instruction.opcode, MachineOpcode::CacheIrGuardShape { shape, .. } if shape == expected_shape)));
            assert!(sequence.instructions().iter().any(|instruction| matches!(instruction.opcode, MachineOpcode::CacheIrStoreField { value_byte, .. } if value_byte == expected_slot)));
        }
    }

    #[test]
    fn property_selection_preserves_exotic_length_program() {
        let mut hir = property_selection_hir();
        let NumericNode::PropertyLoad { exotic_length, .. } = &mut hir.nodes[2] else {
            panic!("property selection fixture load");
        };
        *exotic_length = true;

        let sequence = select(&hir).expect("exotic length Machine IR");
        assert!(sequence.instructions().iter().any(|instruction| {
            matches!(instruction.opcode, MachineOpcode::ExoticLength { .. })
        }));
    }

    #[test]
    fn property_store_cold_call_does_not_create_a_replay_exit() {
        let hir = property_selection_hir();
        let sequence = select(&hir).expect("property Machine IR");
        let allocation = sequence.allocate(&TargetSpec::aarch64()).unwrap();
        let layout = arm64::frame_layout(&allocation, 0).unwrap();
        let table = lower_deopt_table(
            &sequence,
            &allocation,
            layout,
            arm64::GPR_BUDGET,
            arm64::FP_BUDGET,
            &machine_frame_states(&hir),
        )
        .unwrap();
        assert!(table.lookup(0).is_none());
        assert!(
            sequence
                .instructions()
                .iter()
                .all(|instruction| instruction.exits.is_empty())
        );
    }

    #[test]
    fn elements_select_explicit_probe_effect_and_committed_status_cfg() {
        for (access, store) in [
            (NumericElementAccess::Tagged, false),
            (NumericElementAccess::Tagged, true),
            (NumericElementAccess::PackedDouble, false),
        ] {
            let sequence = select(&explicit_element_selection_hir(Some(access), store))
                .expect("explicit element Machine IR");
            let opcodes = sequence
                .instructions()
                .iter()
                .map(|instruction| &instruction.opcode)
                .collect::<Vec<_>>();
            assert!(
                opcodes
                    .iter()
                    .any(|opcode| matches!(opcode, MachineOpcode::ElementView { .. }))
            );
            assert!(
                opcodes
                    .iter()
                    .any(|opcode| matches!(opcode, MachineOpcode::ElementAddress { .. }))
            );
            assert!(
                opcodes
                    .iter()
                    .any(|opcode| matches!(opcode, MachineOpcode::BranchNativeStatus))
            );
            if store {
                let guard = opcodes
                    .iter()
                    .position(|opcode| matches!(opcode, MachineOpcode::ElementValueGuard { .. }))
                    .expect("pre-effect value guard");
                let effect = opcodes
                    .iter()
                    .position(|opcode| matches!(opcode, MachineOpcode::ElementValueStore { .. }))
                    .expect("no-fail element store");
                assert!(guard < effect);
                assert!(sequence.instructions()[effect].exits.is_empty());
            } else {
                assert!(
                    opcodes
                        .iter()
                        .any(|opcode| matches!(opcode, MachineOpcode::ElementValueLoad { .. }))
                );
            }
            let committed = sequence
                .instructions()
                .iter()
                .find_map(|instruction| match instruction.opcode {
                    MachineOpcode::Call(index) => sequence
                        .call_descriptors()
                        .get(index as usize)
                        .filter(|descriptor| {
                            matches!(
                                descriptor.target,
                                CallTarget::CommittedRuntime {
                                    target: otter_vm::native_abi::STUB_JIT_LOAD_ELEMENT
                                        | otter_vm::native_abi::STUB_JIT_STORE_ELEMENT,
                                    ..
                                }
                            )
                        })
                        .map(|_| instruction),
                    _ => None,
                })
                .expect("committed canonical element call");
            assert!(committed.exits.is_empty());
            assert!(committed.safepoint.is_some());
        }
    }

    #[test]
    fn unprepared_element_is_cold_only_without_a_replay_exit() {
        let sequence = select(&explicit_element_selection_hir(None, false))
            .expect("cold-only element Machine IR");
        assert!(sequence.instructions().iter().all(|instruction| {
            !matches!(
                instruction.opcode,
                MachineOpcode::ElementView { .. }
                    | MachineOpcode::ElementAddress { .. }
                    | MachineOpcode::ElementValueLoad { .. }
            )
        }));
        assert!(sequence.instructions().iter().any(|instruction| {
            matches!(instruction.opcode, MachineOpcode::BranchNativeStatus)
        }));
        assert!(sequence.instructions().iter().all(|instruction| {
            !matches!(instruction.opcode, MachineOpcode::Call(_)) || instruction.exits.is_empty()
        }));
    }

    #[test]
    fn executes_ieee_edges_and_boxes_canonical_results() {
        let identity = compile_output(&identity_view(), None).code;
        for value in [f64::INFINITY, f64::NEG_INFINITY, -0.0_f64] {
            let (ret, _, _) = execute(&identity, &[boxed_f64(value)], 0);
            assert_eq!(
                ret.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(
                unbox_number(compiled_payload_bits(ret)).to_bits(),
                value.to_bits()
            );
        }
        let (nan, _, _) = execute(&identity, &[boxed_f64(f64::NAN)], 0);
        assert_eq!(
            nan.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert!(unbox_number(compiled_payload_bits(nan)).is_nan());

        let overflow = compile_output(&overflow_view(), None).code;
        let (ret, _, _) = execute(&overflow, &[tag::box_int32(i32::MAX)], 0);
        assert_eq!(
            ret.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(
            unbox_number(compiled_payload_bits(ret)),
            f64::from(i32::MAX) + 8.0
        );

        let (canonical_int, _, _) = execute(&identity, &[tag::box_int32(42)], 0);
        assert_eq!(compiled_payload_bits(canonical_int), tag::box_int32(42));
    }

    #[test]
    fn small_numeric_leaf_is_not_hidden_behind_a_fixture_size_threshold() {
        let code = compile_output(&small_leaf_view(), None).code;
        assert!(code.metadata().parameter_prefix_entry);
        let (ret, _, _) = execute(&code, &[tag::box_int32(9)], 0);

        assert_eq!(
            ret.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(ret), tag::box_int32(-9));
    }

    #[test]
    fn tagged_parameters_return_without_numeric_guards_or_boxing() {
        let view = tagged_identity_view();
        let hir = NumericFunction::build(&view).expect("tagged identity HIR");
        assert!(matches!(
            hir.nodes[0],
            NumericNode::Parameter {
                register: 0,
                value_type: NumericType::Tagged,
            }
        ));

        let sequence = select(&hir).expect("tagged identity Machine IR");
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .map(|instruction| &instruction.opcode)
                .collect::<Vec<_>>(),
            [&MachineOpcode::EntryValue(0), &MachineOpcode::Return]
        );
        assert_eq!(sequence.representations(), &[MachineRepresentation::Tagged]);

        let code = compile_output(&view, None).code;
        for value in [Value::undefined(), Value::null(), Value::boolean(true)] {
            let (result, _, _) = execute(&code, &[value.to_bits()], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(compiled_payload_bits(result), value.to_bits());
        }
    }

    #[test]
    fn tagged_constants_this_and_bare_return_preserve_exact_value_bits() {
        for (op, expected) in [
            (Op::LoadUndefined, Value::undefined()),
            (Op::LoadNull, Value::null()),
        ] {
            let code = compile_output(&tagged_immediate_view(op), None).code;
            let (result, _, _) = execute(&code, &[], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(compiled_payload_bits(result), expected.to_bits());
        }

        let bare_return = numeric_view(0, 0, vec![(Op::ReturnUndefined, vec![])]);
        let code = compile_output(&bare_return, None).code;
        let (result, _, _) = execute(&code, &[], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), Value::undefined().to_bits());

        let code = compile_output(&tagged_this_view(), None).code;
        let entry: JitEntry = unsafe { std::mem::transmute(code.compiled_code().entry_ptr()) };
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, _, _, _) = execute_at_with_register_count(
            &code,
            entry,
            vec![Value::undefined().to_bits()],
            0,
            code.metadata().register_count,
            Value::null(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), Value::null().to_bits());
    }

    #[test]
    fn tagged_branch_phi_executes_both_edges_without_reencoding_values() {
        let view = tagged_phi_view();
        let hir = NumericFunction::build(&view).expect("tagged branch-phi HIR");
        assert!(hir.blocks.iter().any(|block| {
            block.predecessors.len() == 2
                && block
                    .parameters
                    .iter()
                    .any(|parameter| hir.nodes[parameter.0].value_type() == NumericType::Tagged)
        }));
        let sequence = select(&hir).expect("tagged branch-phi Machine IR");
        assert!(sequence.blocks().iter().any(|block| {
            block.predecessors.len() == 2
                && block.parameters.iter().any(|parameter| {
                    sequence.representations()[parameter.0 as usize]
                        == MachineRepresentation::Tagged
                })
        }));

        let code = compile_output(&view, None).code;
        let (left, _, _) = execute(
            &code,
            &[
                Value::null().to_bits(),
                Value::undefined().to_bits(),
                tag::box_int32(1),
                tag::box_int32(3),
            ],
            0,
        );
        assert_eq!(
            left.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(left), Value::null().to_bits());

        let (right, _, _) = execute(
            &code,
            &[
                Value::null().to_bits(),
                Value::undefined().to_bits(),
                tag::box_int32(3),
                tag::box_int32(1),
            ],
            0,
        );
        assert_eq!(
            right.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(right), Value::undefined().to_bits());
    }

    #[test]
    fn tagged_values_survive_loop_phis_osr_and_backedge_deopt() {
        let view = tagged_loop_view();
        let hir = NumericFunction::build(&view).expect("tagged loop HIR");
        let sequence = select(&hir).expect("tagged loop Machine IR");
        let osr_inputs = sequence
            .instructions()
            .iter()
            .find_map(|instruction| match &instruction.opcode {
                MachineOpcode::OsrEntry {
                    logical_pc: 2,
                    inputs,
                } => Some(inputs.as_slice()),
                _ => None,
            })
            .expect("tagged loop OSR marker");
        assert!(osr_inputs.contains(&MachineOsrInput {
            frame_register: 0,
            value_type: MachineOsrType::Tagged,
        }));

        let code = compile_output(&view, None).code;
        let (normal, _, _) = execute(&code, &[Value::null().to_bits(), tag::box_int32(4)], 0);
        assert_eq!(
            normal.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(normal), Value::null().to_bits());

        let loop_limit = POLL_BATCH + 4;
        let mut frame = vec![Value::undefined().to_bits(); 5];
        frame[0] = Value::boolean(true).to_bits();
        frame[1] = tag::box_int32(loop_limit);
        frame[2] = tag::box_int32(0);
        frame[3] = tag::box_int32(1);
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (osr, after, _) = execute_osr_with_poll_cells(
            &code,
            2,
            frame.clone(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            osr.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(osr), Value::boolean(true).to_bits());
        assert_eq!(after, frame);

        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (bail, after, pc) =
            execute_osr_with_poll_cells(&code, 2, frame, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            bail.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 2);
        assert_eq!(
            after,
            [
                Value::boolean(true).to_bits(),
                tag::box_int32(loop_limit),
                tag::box_int32(POLL_BATCH),
                tag::box_int32(1),
                Value::undefined().to_bits(),
            ],
            "the batched interrupt exit must publish the pre-phi loop state"
        );
    }

    #[test]
    fn tagged_truthiness_has_explicit_probe_and_cold_leaf_cfg() {
        let target = TargetSpec::aarch64();
        let view = tagged_truthiness_branch_view();
        let hir = NumericFunction::build(&view).expect("tagged truthiness HIR");
        assert_eq!(hir.frame_states.len(), 1);
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::TaggedToBoolean(_)))
        );

        let sequence = select(&hir).expect("tagged truthiness Machine IR");
        assert_eq!(sequence.call_descriptors().len(), 1);
        let probe_block = sequence
            .blocks()
            .iter()
            .find(|block| {
                sequence.instructions()[block.first.0 as usize..block.end.0 as usize]
                    .iter()
                    .any(|i| i.opcode == MachineOpcode::TruthinessProbe)
            })
            .expect("explicit fast truthiness block");
        assert_eq!(probe_block.successors.len(), 2);
        let fast = &sequence.blocks()[probe_block.successors[0].0 as usize];
        let cold = &sequence.blocks()[probe_block.successors[1].0 as usize];
        assert_eq!(fast.successors, cold.successors, "both results join");
        assert!(
            sequence.instructions()[fast.first.0 as usize..fast.end.0 as usize]
                .iter()
                .all(|i| !matches!(i.opcode, MachineOpcode::Call(_)))
        );
        assert!(matches!(
            sequence.instructions()[cold.first.0 as usize].opcode,
            MachineOpcode::Call(0)
        ));
        let descriptor = &sequence.call_descriptors()[0];
        assert_eq!(
            descriptor.target,
            CallTarget::RuntimeStub(otter_vm::native_abi::STUB_TO_BOOLEAN_LEAF)
        );
        assert_eq!(
            descriptor.arguments,
            [MachineRepresentation::Tagged, MachineRepresentation::Tagged]
        );
        assert_eq!(descriptor.results, [MachineRepresentation::Boolean]);
        assert_eq!(descriptor.effects, CallEffects::READS_HEAP);
        assert_eq!(descriptor.exceptional, ExceptionalEdge::None);
        assert_eq!(descriptor.safepoint, SafepointKind::None);

        let (call_id, call) = sequence
            .instructions()
            .iter()
            .enumerate()
            .find(|(_, instruction)| matches!(instruction.opcode, MachineOpcode::Call(0)))
            .map(|(index, instruction)| (MachineInstructionId(index as u32), instruction))
            .expect("tagged truthiness call");
        assert_eq!(
            call.operands[0].constraint,
            OperandConstraint::Fixed(target.integer_argument(1).expect("argument 1"))
        );
        assert_eq!(
            call.operands[2].constraint,
            OperandConstraint::Fixed(target.integer_result())
        );
        assert!(call.deopt_id().is_some());
        assert_eq!(
            call.operands
                .iter()
                .filter(|operand| operand.purpose == OperandPurpose::FrameState)
                .count(),
            3
        );
        let allocation = sequence
            .allocate(&target)
            .expect("tagged truthiness allocation");
        let locations = allocation
            .instruction_locations(call_id)
            .expect("tagged truthiness locations");
        assert_eq!(
            locations[0],
            AllocatedLocation::Register(target.integer_argument(1).expect("argument 1"))
        );
        assert_eq!(
            locations[2],
            AllocatedLocation::Register(target.integer_result())
        );
    }

    #[test]
    fn tagged_truthiness_executes_all_immediate_classes_and_exact_miss_deopt() {
        let code = compile_output(&tagged_truthiness_branch_view(), None).code;
        let selected = Value::null().to_bits();
        let rejected = Value::undefined().to_bits();
        for condition in [
            Value::boolean(true).to_bits(),
            tag::box_int32(1),
            boxed_f64(2.5),
        ] {
            let (result, _, _) = execute(&code, &[condition, selected, rejected], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(compiled_payload_bits(result), selected);
        }
        for condition in [
            Value::boolean(false).to_bits(),
            Value::null().to_bits(),
            Value::undefined().to_bits(),
            tag::box_int32(0),
            boxed_f64(-0.0),
            boxed_f64(f64::NAN),
        ] {
            let (result, _, _) = execute(&code, &[condition, selected, rejected], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(compiled_payload_bits(result), rejected);
        }

        let logical_not = compile_output(&tagged_logical_not_view(), None).code;
        for (condition, expected) in [
            (Value::null().to_bits(), true),
            (Value::boolean(false).to_bits(), true),
            (tag::box_int32(7), false),
        ] {
            let (result, _, _) = execute(&logical_not, &[condition], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(
                compiled_payload_bits(result),
                Value::boolean(expected).to_bits()
            );
        }

        let entry: JitEntry = unsafe { std::mem::transmute(code.compiled_code().entry_ptr()) };
        let frame = vec![Value::hole().to_bits(), selected, rejected];
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, after, pc, register_count) = execute_at_with_heap(
            &code,
            entry,
            frame.clone(),
            91,
            code.metadata().param_count,
            Value::undefined(),
            std::ptr::null(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 0);
        assert_eq!(register_count, code.metadata().register_count);
        assert_eq!(after, frame);
    }

    #[test]
    fn tagged_strict_equality_uses_verified_leaf_call_and_boxes_scalars() {
        let target = TargetSpec::aarch64();
        let view = tagged_mixed_strict_equality_view();
        let hir = NumericFunction::build(&view).expect("tagged strict equality HIR");
        assert_eq!(hir.frame_states.len(), 1);
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::TaggedStrictEqual(..)))
        );

        let sequence = select(&hir).expect("tagged strict equality Machine IR");
        assert_eq!(sequence.call_descriptors().len(), 1);
        let descriptor = &sequence.call_descriptors()[0];
        assert_eq!(
            descriptor.target,
            CallTarget::RuntimeStub(otter_vm::native_abi::STUB_STRICT_EQ_LEAF)
        );
        assert_eq!(
            descriptor.arguments,
            [MachineRepresentation::Tagged, MachineRepresentation::Tagged]
        );
        assert_eq!(descriptor.results, [MachineRepresentation::Boolean]);
        assert_eq!(descriptor.effects, CallEffects::READS_HEAP);
        assert_eq!(descriptor.exceptional, ExceptionalEdge::None);
        assert_eq!(descriptor.safepoint, SafepointKind::None);
        assert!(
            sequence
                .instructions()
                .iter()
                .any(|instruction| instruction.opcode == MachineOpcode::BoxInt32)
        );

        let (call_id, call) = sequence
            .instructions()
            .iter()
            .enumerate()
            .find(|(_, instruction)| matches!(instruction.opcode, MachineOpcode::Call(0)))
            .map(|(index, instruction)| (MachineInstructionId(index as u32), instruction))
            .expect("tagged strict equality call");
        let expected_registers = [
            target.integer_argument(1).expect("argument 1"),
            target.integer_argument(2).expect("argument 2"),
            target.integer_result(),
        ];
        for (operand, register) in call.operands[..3].iter().zip(expected_registers) {
            assert_eq!(operand.constraint, OperandConstraint::Fixed(register));
        }
        assert!(call.deopt_id().is_some());
        let allocation = sequence
            .allocate(&target)
            .expect("tagged strict equality allocation");
        let locations = allocation
            .instruction_locations(call_id)
            .expect("tagged strict equality locations");
        for (&location, register) in locations[..3].iter().zip(expected_registers) {
            assert_eq!(location, AllocatedLocation::Register(register));
        }
    }

    #[test]
    fn tagged_strict_equality_executes_full_number_semantics_and_exact_miss_deopt() {
        let code = compile_output(&tagged_strict_equality_view(Op::Equal), None).code;
        for (left, right, expected) in [
            (Value::null().to_bits(), Value::null().to_bits(), true),
            (
                Value::undefined().to_bits(),
                Value::undefined().to_bits(),
                true,
            ),
            (
                Value::boolean(true).to_bits(),
                Value::boolean(true).to_bits(),
                true,
            ),
            (tag::box_int32(7), tag::box_int32(7), true),
            (tag::box_int32(7), boxed_f64(7.0), true),
            (boxed_f64(0.0), boxed_f64(-0.0), true),
            (boxed_f64(f64::NAN), boxed_f64(f64::NAN), false),
            (Value::null().to_bits(), Value::undefined().to_bits(), false),
            (Value::boolean(true).to_bits(), tag::box_int32(1), false),
        ] {
            let (result, _, _) = execute(&code, &[left, right], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(
                compiled_payload_bits(result),
                Value::boolean(expected).to_bits()
            );
        }

        let not_equal = compile_output(&tagged_strict_equality_view(Op::NotEqual), None).code;
        let (result, _, _) = execute(&not_equal, &[tag::box_int32(7), boxed_f64(8.0)], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(
            compiled_payload_bits(result),
            Value::boolean(true).to_bits()
        );

        let mixed = compile_output(&tagged_mixed_strict_equality_view(), None).code;
        let (result, _, _) = execute(&mixed, &[boxed_f64(7.0)], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(
            compiled_payload_bits(result),
            Value::boolean(true).to_bits()
        );

        let entry: JitEntry = unsafe { std::mem::transmute(code.compiled_code().entry_ptr()) };
        let frame = vec![
            tag::box_int32(3),
            tag::box_int32(4),
            Value::undefined().to_bits(),
        ];
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, after, pc, register_count) = execute_at_with_heap(
            &code,
            entry,
            frame.clone(),
            17,
            code.metadata().param_count,
            Value::undefined(),
            std::ptr::null(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 0);
        assert_eq!(register_count, code.metadata().register_count);
        assert_eq!(after, frame);
    }

    #[test]
    fn tagged_string_concat_uses_allocator_driven_vm_safepoints() {
        let view = tagged_string_concat_view(3);
        let hir = NumericFunction::build(&view).expect("tagged string-concat HIR");
        assert_eq!(
            hir.nodes
                .iter()
                .filter(|node| matches!(node, NumericNode::TaggedStringConcat(..)))
                .count(),
            2
        );
        assert_eq!(hir.frame_states.len(), 2);

        let sequence = select(&hir).expect("tagged string-concat Machine IR");
        assert_eq!(sequence.call_descriptors().len(), 1);
        let descriptor = &sequence.call_descriptors()[0];
        assert_eq!(
            descriptor.target,
            CallTarget::RuntimeStub(otter_vm::native_abi::STUB_STRING_CONCAT_ALLOC)
        );
        assert_eq!(descriptor.arguments, [MachineRepresentation::Tagged; 3]);
        assert_eq!(descriptor.results, [MachineRepresentation::Tagged]);
        assert_eq!(descriptor.safepoint, SafepointKind::Gc);

        let calls = sequence
            .instructions()
            .iter()
            .enumerate()
            .filter(|(_, instruction)| matches!(instruction.opcode, MachineOpcode::Call(0)))
            .map(|(index, instruction)| (MachineInstructionId(index as u32), instruction))
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1.safepoint, Some(SafepointId(0)));
        assert_eq!(calls[1].1.safepoint, Some(SafepointId(1)));
        assert!(calls.iter().all(|(_, call)| call.deopt_id().is_some()));
        assert!(calls.iter().all(|(_, call)| {
            call.operands
                .iter()
                .any(|operand| operand.purpose == OperandPurpose::TaggedRoot)
        }));

        let allocation = sequence
            .allocate(&TargetSpec::aarch64())
            .expect("tagged string-concat allocation");
        let safepoints =
            lower_safepoints(&sequence, &allocation).expect("allocator-driven tagged safepoints");
        assert_eq!(safepoints.records().len(), 2);
        assert_eq!(safepoints.records()[0].id, 0);
        assert_eq!(safepoints.records()[0].frame_state, 0);
        assert_eq!(safepoints.records()[1].id, 1);
        assert_eq!(safepoints.records()[1].frame_state, 1);
        assert!(safepoints.records().iter().all(|record| {
            !record.tagged_locations.is_empty()
                && record.tagged_locations.iter().all(|location| {
                    location.kind == otter_vm::native_abi::TaggedLocationKind::SpillSlot
                })
        }));

        let code = compile_output(&view, None).code;
        assert!(!code.metadata().parameter_prefix_entry);
        assert_eq!(JitFunctionCode::safepoint_count(&code), 2);
        assert_eq!(
            code.deopt_table()
                .entries()
                .next()
                .unwrap()
                .outermost()
                .byte_pc,
            0
        );
        assert_eq!(
            code.deopt_table()
                .entries()
                .nth(1)
                .unwrap()
                .outermost()
                .byte_pc,
            8
        );
    }

    #[test]
    fn tagged_string_concat_forces_roots_into_allocator_spills() {
        let hir = NumericFunction::build(&tagged_string_concat_view(16))
            .expect("pressure string-concat HIR");
        let sequence = select(&hir).expect("pressure string-concat Machine IR");
        let allocation = sequence
            .allocate(&TargetSpec::aarch64())
            .expect("pressure string-concat allocation");
        let safepoints =
            lower_safepoints(&sequence, &allocation).expect("pressure allocator-driven safepoints");
        let first_call = sequence
            .instructions()
            .iter()
            .position(|instruction| matches!(instruction.opcode, MachineOpcode::Call(0)))
            .map(|index| MachineInstructionId(index as u32))
            .expect("first pressure concat call");
        let first_site = safepoints
            .site(first_call)
            .expect("first pressure safepoint");
        assert!(first_site.roots.len() > 9);
        assert!(
            first_site
                .roots
                .iter()
                .any(|root| matches!(root.source, AllocatedLocation::Stack(_))),
            "callee-saved GPR pressure must force at least one GC root to a spill"
        );
        let frame = arm64::frame_layout(&allocation, safepoints.root_slot_count())
            .expect("pressure root-save frame");
        assert_eq!(frame.root_slots(), safepoints.root_slot_count());
        assert!(frame.root_offset(0).expect("first root offset") >= allocation.spill_slots() * 8);
    }

    #[test]
    fn feedback_specializes_numeric_parameters_until_a_float64_boundary() {
        let view = typed_parameter_leaf_view();
        let hir = NumericFunction::build(&view).expect("typed parameter numeric HIR");
        assert!(matches!(
            hir.nodes[0],
            NumericNode::Parameter {
                register: 0,
                value_type: NumericType::Int32
            }
        ));
        assert!(matches!(
            hir.nodes[1],
            NumericNode::Parameter {
                register: 1,
                value_type: NumericType::Int32
            }
        ));
        assert_eq!(
            hir.nodes
                .iter()
                .filter(|node| matches!(node, NumericNode::WidenInt32(..)))
                .count(),
            2
        );

        let sequence = select(&hir).expect("typed parameter Machine IR");
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| instruction.opcode == MachineOpcode::DecodeInt32)
                .count(),
            2
        );
        assert!(sequence.instructions().iter().all(|instruction| {
            instruction.opcode != MachineOpcode::DecodeInt32
                || instruction.operands[1].constraint == OperandConstraint::Reuse(0)
        }));
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter(|instruction| {
                    matches!(
                        instruction.opcode,
                        MachineOpcode::IntegerAdd
                            | MachineOpcode::IntegerSub
                            | MachineOpcode::IntegerMul
                    )
                })
                .count(),
            6
        );

        let code = compile_output(&view, None).code;
        let (result, _, _) = execute(&code, &[tag::box_int32(2), tag::box_int32(2)], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(-7));

        let (bail, frame, pc) = execute(&code, &[boxed_f64(2.5), tag::box_int32(2)], 77);
        assert_eq!(
            bail.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 0);
        assert_eq!(frame[0], boxed_f64(2.5));
        assert_eq!(frame[1], tag::box_int32(2));
    }

    #[test]
    fn parameter_inference_tracks_copy_aliases_without_specializing_bitwise_coercions() {
        let alias = NumericFunction::build(&typed_parameter_alias_view())
            .expect("copy-alias typed parameter HIR");
        assert!(matches!(
            alias.nodes[0],
            NumericNode::Parameter {
                value_type: NumericType::Int32,
                ..
            }
        ));
        let (result, _, _) = execute(
            &compile_output(&typed_parameter_alias_view(), None).code,
            &[tag::box_int32(41)],
            0,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(42));

        let bitwise = NumericFunction::build(&float_bitwise_view(Op::BitwiseAnd))
            .expect("bitwise numeric HIR");
        let parameters = [hir::NumericValue(0), hir::NumericValue(1)];
        for parameter in parameters {
            assert!(matches!(
                bitwise.nodes[parameter.0],
                NumericNode::Parameter {
                    value_type: NumericType::Tagged,
                    ..
                }
            ));
            let number = bitwise
                .nodes
                .iter()
                .position(|node| *node == NumericNode::TaggedToNumber(parameter))
                .map(hir::NumericValue)
                .expect("exact tagged-number decode");
            assert!(bitwise.nodes.contains(&NumericNode::FloatToInt32(number)));
        }
    }

    #[test]
    fn typed_parameter_overflow_reconstructs_the_exact_entry_frame() {
        let code = compile_output(&typed_parameter_overflow_view(), None).code;
        let (result, frame, pc) =
            execute(&code, &[tag::box_int32(i32::MAX), tag::box_int32(1)], 91);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 0);
        assert_eq!(
            frame,
            [
                tag::box_int32(i32::MAX),
                tag::box_int32(1),
                Value::undefined().to_bits()
            ]
        );
    }

    #[test]
    fn parameter_prefix_cold_exits_publish_a_complete_vm_window() {
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;

        let guard = compile_output(&small_leaf_view(), None).code;
        let guard_entry: JitEntry =
            unsafe { std::mem::transmute(guard.compiled_code().entry_ptr()) };
        let (result, frame, _, register_count) = execute_at_with_register_count(
            &guard,
            guard_entry,
            vec![tag::box_int32(9), 0xdead_beef_dead_beef],
            91,
            guard.metadata().param_count,
            Value::undefined(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(-9));
        assert_eq!(register_count, guard.metadata().param_count);
        assert_eq!(frame[1], 0xdead_beef_dead_beef);

        let (result, frame, pc, register_count) = execute_at_with_register_count(
            &guard,
            guard_entry,
            vec![Value::undefined().to_bits(), 0xdead_beef_dead_beef],
            91,
            guard.metadata().param_count,
            Value::undefined(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 0);
        assert_eq!(register_count, guard.metadata().register_count);
        assert_eq!(frame[1], Value::undefined().to_bits());

        let overflow = compile_output(&typed_parameter_overflow_view(), None).code;
        let overflow_entry: JitEntry =
            unsafe { std::mem::transmute(overflow.compiled_code().entry_ptr()) };
        let (result, frame, pc, register_count) = execute_at_with_register_count(
            &overflow,
            overflow_entry,
            vec![
                tag::box_int32(i32::MAX),
                tag::box_int32(1),
                0xdead_beef_dead_beef,
            ],
            91,
            overflow.metadata().param_count,
            Value::undefined(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 0);
        assert_eq!(register_count, overflow.metadata().register_count);
        assert_eq!(
            frame,
            [
                tag::box_int32(i32::MAX),
                tag::box_int32(1),
                Value::undefined().to_bits()
            ]
        );
    }

    #[test]
    fn dead_parameters_have_no_entry_load_guard_or_allocator_value() {
        let view = unused_parameter_view();
        let hir = NumericFunction::build(&view).expect("live-only parameter HIR");
        assert_eq!(
            hir.nodes
                .iter()
                .filter_map(|node| match node {
                    NumericNode::Parameter { register, .. } => Some(*register),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [1]
        );
        let sequence = select(&hir).expect("live-only parameter Machine IR");
        assert_eq!(
            sequence
                .instructions()
                .iter()
                .filter_map(|instruction| match instruction.opcode {
                    MachineOpcode::EntryValue(parameter) => Some(parameter),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [1]
        );

        let code = compile_output(&view, None).code;
        let (result, _, _) = execute(&code, &[Value::undefined().to_bits(), tag::box_int32(9)], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(-9));
    }

    #[test]
    fn typed_parameters_publish_a_complete_integer_loop() {
        let view = typed_parameter_loop_view();
        let hir = NumericFunction::build(&view).expect("typed parameter loop HIR");
        assert!(hir.nodes[..2].iter().all(|node| matches!(
            node,
            NumericNode::Parameter {
                value_type: NumericType::Int32,
                ..
            }
        )));
        let sequence = select(&hir).expect("typed parameter loop Machine IR");
        sequence
            .allocate(&TargetSpec::aarch64())
            .expect("typed parameter loop allocation");

        let code = compile_output(&view, None).code;
        let (result, _, _) = execute(&code, &[tag::box_int32(10), tag::box_int32(3)], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(30));
        // SAFETY: the code object remains alive for the pointer lookup.
        assert!(unsafe { code.osr_entry_ptr_for_test(2) }.is_some());
    }

    #[test]
    fn numeric_loop_join_widens_the_int32_entry_edge() {
        let mut view = typed_parameter_loop_view();
        view.seed_arith_feedback_for_test(4, ArithFeedback::from_bits(ARITH_INT32 | ARITH_FLOAT64));
        let hir = NumericFunction::build(&view).expect("mixed numeric loop HIR");
        let sequence = select(&hir).expect("mixed numeric loop Machine IR");
        assert!(sequence.instructions().iter().any(|instruction| {
            instruction.opcode == MachineOpcode::Int32ToFloat64 && instruction.exits.is_empty()
        }));
    }

    #[test]
    fn splits_noncritical_edges_for_every_supported_representation_conversion() {
        for (source_type, target_type, expected_opcode) in [
            (
                NumericType::Int32,
                NumericType::Number,
                MachineOpcode::Int32ToFloat64,
            ),
            (
                NumericType::Uint32,
                NumericType::Number,
                MachineOpcode::Uint32ToFloat64,
            ),
            (
                NumericType::Int32,
                NumericType::Tagged,
                MachineOpcode::BoxInt32,
            ),
            (
                NumericType::Uint32,
                NumericType::Tagged,
                MachineOpcode::BoxUint32,
            ),
            (
                NumericType::Number,
                NumericType::Tagged,
                MachineOpcode::BoxNumber,
            ),
            (
                NumericType::Boolean,
                NumericType::Tagged,
                MachineOpcode::BoxBoolean,
            ),
        ] {
            let (hir, source, parameter) = edge_conversion_hir(source_type, target_type);
            let sequence = select(&hir).expect("convertible edge Machine IR");
            assert_eq!(
                sequence.blocks().len(),
                hir.blocks.len() + 1,
                "{source_type:?} -> {target_type:?} must split a noncritical edge"
            );
            let split = sequence
                .blocks()
                .iter()
                .find(|block| {
                    let instructions =
                        &sequence.instructions()[block.first.0 as usize..block.end.0 as usize];
                    instructions
                        .first()
                        .is_some_and(|instruction| instruction.opcode == expected_opcode)
                })
                .expect("representation-conversion split block");
            let instructions =
                &sequence.instructions()[split.first.0 as usize..split.end.0 as usize];
            assert_eq!(instructions.len(), 2);
            assert_eq!(instructions[0].opcode, expected_opcode);
            assert_eq!(instructions[1].opcode, MachineOpcode::Jump);
            assert_eq!(
                instructions[0].operands[0],
                MachineOperand::register_input(MachineValue(source.0 as u32))
            );
            let converted = instructions[0].operands[1].value;
            assert_ne!(converted, MachineValue(source.0 as u32));
            assert_eq!(split.successor_arguments, [vec![converted]]);
            assert_eq!(
                sequence.representations()[converted.0 as usize],
                match target_type {
                    NumericType::Tagged => MachineRepresentation::Tagged,
                    NumericType::Number => MachineRepresentation::Float64,
                    _ => unreachable!("conversion targets are Tagged or Number"),
                }
            );
            let successor = &sequence.blocks()[split.successors[0].0 as usize];
            assert_eq!(successor.parameters, [MachineValue(parameter.0 as u32)]);
            let predecessor = &sequence.blocks()[split.predecessors[0].0 as usize];
            assert!(predecessor.successor_arguments[0].is_empty());
            sequence
                .allocate(&TargetSpec::aarch64())
                .expect("representation-conversion allocation");
        }
    }

    #[test]
    fn rejects_an_unapproved_edge_representation_conversion() {
        let (hir, _, _) = edge_conversion_hir(NumericType::Tagged, NumericType::Number);
        let Err(error) = select(&hir) else {
            panic!("Tagged -> Number edge conversion must be rejected")
        };
        assert!(matches!(
            error,
            VerificationError::BlockParameterRepresentation(..)
        ));
    }

    #[test]
    fn backedge_poll_precedes_boxing_and_keeps_the_original_exact_state() {
        let hir = tagged_backedge_conversion_hir();
        let sequence = select(&hir).expect("tagged mixed-backedge Machine IR");
        let split = sequence
            .blocks()
            .iter()
            .find(|block| {
                sequence.instructions()[block.first.0 as usize].opcode
                    == MachineOpcode::BackedgePoll
            })
            .expect("backedge split block");
        let instructions = &sequence.instructions()[split.first.0 as usize..split.end.0 as usize];
        assert_eq!(instructions.len(), 3);
        assert_eq!(instructions[0].opcode, MachineOpcode::BackedgePoll);
        assert_eq!(instructions[1].opcode, MachineOpcode::BoxInt32);
        assert_eq!(instructions[2].opcode, MachineOpcode::Jump);
        assert_eq!(instructions[0].deopt_id(), Some(DeoptId(0)));
        assert_eq!(
            instructions[0]
                .operands
                .iter()
                .map(|operand| (operand.value, operand.purpose))
                .collect::<Vec<_>>(),
            [(MachineValue(2), OperandPurpose::FrameState)]
        );
        assert_eq!(instructions[1].operands[0].value, MachineValue(2));
        let converted = instructions[1].operands[1].value;
        assert!(
            instructions[0]
                .operands
                .iter()
                .all(|operand| operand.value != converted),
            "the backedge frame must not observe the post-poll boxed value"
        );
        assert_eq!(split.successor_arguments, [vec![converted]]);
        assert_eq!(
            sequence.representations()[converted.0 as usize],
            MachineRepresentation::Tagged
        );
        sequence
            .allocate(&TargetSpec::aarch64())
            .expect("tagged backedge conversion allocation");
    }

    #[test]
    fn executes_allocator_spills_through_the_shared_frame_layout() {
        let code = compile_output(&spill_pressure_view(), None).code;
        assert!(code.metadata().spill_slot_count > 0);
        assert!(
            JitFunctionCode::generated_stack_frame_bytes(&code)
                .is_some_and(|frame_bytes| frame_bytes > 16)
        );

        let (ret, _, _) = execute(&code, &[tag::box_int32(0)], 0);
        assert_eq!(
            ret.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(ret), tag::box_int32(528));

        let (bail, frame, pc) = execute(&code, &[Value::undefined().to_bits()], 77);
        assert_eq!(
            bail.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 0);
        assert_eq!(frame[0], Value::undefined().to_bits());
    }

    #[test]
    fn executes_numeric_diamond_with_allocator_block_parameter() {
        let view = diamond_view(Op::JumpIfFalse);
        let hir = NumericFunction::build(&view).expect("diamond numeric HIR");
        let sequence = select(&hir).expect("diamond Machine IR");
        let merge = sequence
            .blocks()
            .iter()
            .find(|block| block.predecessors.len() == 2)
            .expect("diamond merge block");
        assert_eq!(merge.parameters.len(), 1);
        assert!(
            sequence
                .blocks()
                .iter()
                .filter(|block| block.successors.contains(&MachineBlock(3)))
                .all(|block| block.successor_arguments[0].len() == 1)
        );

        let code = compile_output(&view, None).code;

        let (taken, _, _) = execute(&code, &[tag::box_int32(1), tag::box_int32(3)], 0);
        assert_eq!(
            taken.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(taken), tag::box_int32(4));

        let (fallthrough, _, _) = execute(&code, &[tag::box_int32(3), tag::box_int32(1)], 0);
        assert_eq!(
            fallthrough.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(fallthrough), tag::box_int32(2));

        let (unordered, _, _) = execute(&code, &[boxed_f64(f64::NAN), tag::box_int32(1)], 0);
        assert_eq!(
            unordered.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert!(unbox_number(compiled_payload_bits(unordered)).is_nan());

        let (bail, frame, pc) = execute(
            &code,
            &[tag::box_int32(1), Value::undefined().to_bits()],
            19,
        );
        assert_eq!(
            bail.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 0);
        assert_eq!(frame[1], Value::undefined().to_bits());

        let branch_true = compile_output(&diamond_view(Op::JumpIfTrue), None).code;
        let (true_edge, _, _) = execute(&branch_true, &[tag::box_int32(1), tag::box_int32(3)], 0);
        assert_eq!(compiled_payload_bits(true_edge), tag::box_int32(-2));
        let (false_edge, _, _) = execute(&branch_true, &[tag::box_int32(3), tag::box_int32(1)], 0);
        assert_eq!(compiled_payload_bits(false_edge), tag::box_int32(4));
    }

    #[test]
    fn splits_and_executes_critical_edge_with_block_parameter() {
        let view = critical_edge_view();
        let hir = NumericFunction::build(&view).expect("critical-edge numeric HIR");
        let sequence = select(&hir).expect("split critical-edge Machine IR");
        assert_eq!(sequence.blocks().len(), hir.blocks.len() + 1);
        let split = sequence
            .blocks()
            .iter()
            .find(|block| {
                block.predecessors.len() == 1
                    && block.successors.len() == 1
                    && block.parameters.is_empty()
                    && block.successor_arguments[0].len() == 1
            })
            .expect("critical edge split block");
        assert!(
            sequence.blocks()[split.successors[0].0 as usize]
                .parameters
                .len()
                == 1
        );

        let code = compile_output(&view, None).code;
        let (direct_edge, _, _) = execute(&code, &[tag::box_int32(3), tag::box_int32(1)], 0);
        assert_eq!(
            direct_edge.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(direct_edge), tag::box_int32(3));

        let (arm_edge, _, _) = execute(&code, &[tag::box_int32(1), tag::box_int32(3)], 0);
        assert_eq!(
            arm_edge.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(arm_edge), tag::box_int32(4));
    }

    #[test]
    fn executes_loop_header_parameters_through_native_publication() {
        let view = loop_view();
        let hir = NumericFunction::build(&view).expect("loop numeric HIR");
        assert!(
            hir.frame_states
                .iter()
                .any(|state| matches!(state.point, NumericFramePoint::Backedge { .. }))
        );
        let sequence = select(&hir).expect("loop Machine IR");
        let (header_index, header) = sequence
            .blocks()
            .iter()
            .enumerate()
            .find(|(_, block)| block.predecessors.len() == 2 && !block.parameters.is_empty())
            .expect("loop header block parameters");
        assert_eq!(header.parameters.len(), 2);
        for &predecessor in &header.predecessors {
            let predecessor = &sequence.blocks()[predecessor.0 as usize];
            let edge = predecessor
                .successors
                .iter()
                .position(|successor| successor.0 as usize == header_index)
                .expect("incoming loop edge");
            assert_eq!(predecessor.successor_arguments[edge].len(), 2);
        }
        sequence
            .allocate(&TargetSpec::aarch64())
            .expect("loop Machine IR allocation");

        let code = compile_output(&view, None).code;
        assert!(!code.metadata().parameter_prefix_entry);
        for (input, expected) in [(-2, 1), (0, 1), (1, 1), (3, 3)] {
            let (result, _, _) = execute(&code, &[tag::box_int32(input)], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(compiled_payload_bits(result), tag::box_int32(expected));
        }
    }

    #[test]
    fn executes_branch_phi_integer_loop_through_machine_ir_backend() {
        let view = branch_phi_loop_view();
        let hir = NumericFunction::build(&view).expect("branch-phi numeric HIR");
        assert_eq!(hir.frame_states.len(), 3);
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerAndImmediate(_, 1)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerAddImmediate(_, 1)))
        );
        let sequence = select(&hir).expect("branch-phi Machine IR");
        let (osr_logical_pc, osr_inputs, osr_operands) = sequence
            .instructions()
            .iter()
            .find_map(|instruction| match &instruction.opcode {
                MachineOpcode::OsrEntry { logical_pc, inputs } => Some((
                    *logical_pc,
                    inputs.as_slice(),
                    instruction.operands.as_slice(),
                )),
                _ => None,
            })
            .expect("branch-phi OSR marker");
        assert_eq!(osr_logical_pc, 3);
        assert_eq!(
            osr_inputs,
            [
                MachineOsrInput {
                    frame_register: 0,
                    value_type: MachineOsrType::Int32,
                },
                MachineOsrInput {
                    frame_register: 1,
                    value_type: MachineOsrType::Int32,
                },
            ]
        );
        assert_eq!(osr_operands.len(), osr_inputs.len());
        assert!(osr_operands.iter().all(|operand| {
            operand.constraint == OperandConstraint::Any && operand.timing == OperandTiming::Late
        }));
        let polls = sequence
            .blocks()
            .iter()
            .filter_map(|block| {
                let instructions =
                    &sequence.instructions()[block.first.0 as usize..block.end.0 as usize];
                instructions
                    .iter()
                    .any(|instruction| instruction.opcode == MachineOpcode::BackedgePoll)
                    .then_some((block, instructions))
            })
            .collect::<Vec<_>>();
        assert_eq!(polls.len(), 1);
        let (poll_block, poll_instructions) = polls[0];
        assert_eq!(poll_block.predecessors.len(), 1);
        assert_eq!(poll_block.successors.len(), 1);
        assert!(poll_block.parameters.is_empty());
        assert!(matches!(
            poll_instructions
                .last()
                .map(|instruction| &instruction.opcode),
            Some(MachineOpcode::Jump)
        ));
        let poll = poll_instructions
            .iter()
            .find(|instruction| instruction.opcode == MachineOpcode::BackedgePoll)
            .expect("backedge poll instruction");
        assert_eq!(poll.deopt_id(), Some(DeoptId(2)));
        assert_eq!(poll.operands.len(), 2);
        assert!(sequence.blocks().iter().any(|block| {
            block.parameters.len() >= 2
                && block.parameters.iter().all(|parameter| {
                    sequence.representations()[parameter.0 as usize] == MachineRepresentation::Int32
                })
        }));
        let allocation = sequence
            .allocate(&TargetSpec::aarch64())
            .expect("branch-phi Machine IR allocation");
        assert!(
            allocation
                .metadata()
                .iter()
                .filter(|metadata| metadata.frame_state == Some(2))
                .all(|metadata| match metadata.location {
                    AllocatedLocation::Stack(_) => true,
                    AllocatedLocation::Register(register) => {
                        register.is_integer() && (20..=28).contains(&register.encoding())
                    }
                }),
            "poll operands must survive the leaf call outside caller-saved registers"
        );
        assert!(allocation.metadata().iter().any(|metadata| {
            metadata.frame_state == Some(2)
                && matches!(metadata.location, AllocatedLocation::Register(register)
                    if register.is_integer() && (20..=28).contains(&register.encoding()))
        }));
        let layout = arm64::frame_layout(&allocation, 0).expect("branch-phi frame layout");
        let deopt_table = lower_deopt_table(
            &sequence,
            &allocation,
            layout,
            arm64::GPR_BUDGET,
            arm64::FP_BUDGET,
            &machine_frame_states(&hir),
        )
        .expect("branch-phi allocator-driven FrameState");
        assert_eq!(deopt_table.len(), 3);
        assert_eq!(
            deopt_table
                .entries()
                .next()
                .unwrap()
                .outermost()
                .slots
                .len(),
            12
        );
        assert_eq!(
            deopt_table
                .entries()
                .nth(1)
                .unwrap()
                .outermost()
                .slots
                .len(),
            12
        );
        assert_eq!(
            deopt_table
                .entries()
                .nth(2)
                .unwrap()
                .outermost()
                .slots
                .len(),
            12
        );
        let live_slot_counts = deopt_table
            .entries()
            .map(|state| {
                state
                    .outermost()
                    .slots
                    .iter()
                    .filter(|slot| {
                        !matches!(slot.location, otter_vm::deopt::DeoptLocation::Literal(_))
                    })
                    .count()
            })
            .collect::<Vec<_>>();
        assert_eq!(live_slot_counts, [3, 2, 2]);

        for (limit, expected) in [(1, 2), (2, -12), (5, -22)] {
            let code = compile_output(&branch_phi_loop_view_with(0, 0, limit, 1), None).code;
            let (result, _, _) = execute(&code, &[], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(compiled_payload_bits(result), tag::box_int32(expected));
        }

        let transitions = TransitionTable::resolve();
        let exact = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7003,
            &transitions,
            false,
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "engineKernel".to_string(),
                    module: "benchmarks/scripts/branch-phi.js".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
        )
        .expect("production optimizing selector compiles exact branch-phi");
        let artifact = exact.artifact.expect("exact branch-phi artifact");
        let optimized_ir = std::str::from_utf8(
            artifact
                .file(JitArtifactFileName::OptimizedIr)
                .expect("optimized IR artifact")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));
        assert!(optimized_ir.contains("OsrEntry { logical_pc: 3"));
        let code_map = std::str::from_utf8(
            artifact
                .file(JitArtifactFileName::CodeMap)
                .expect("branch-phi code map")
                .contents(),
        )
        .expect("UTF-8 branch-phi code map");
        assert!(code_map.contains("\"logicalPc\": 3"));
        let (result, _, _) = execute(&exact.code, &[], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(-6_000_000));
    }

    #[test]
    fn executes_countdown_integer_loop_through_machine_ir_backend() {
        let view = countdown_loop_view();
        let hir = NumericFunction::build(&view).expect("countdown numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerSub(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerSubImmediate(_, -3)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerAddImmediate(_, 1)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerNotEqualImmediate(_, 0)))
        );
        assert_eq!(hir.frame_states.len(), 4);

        let output = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7004,
            &TransitionTable::resolve(),
            false,
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "countdownKernel".to_string(),
                    module: "test:countdown-machine-loop".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
        )
        .expect("production optimizing selector compiles countdown loop");
        let optimized_ir = std::str::from_utf8(
            output
                .artifact
                .as_ref()
                .expect("countdown artifact")
                .file(JitArtifactFileName::OptimizedIr)
                .expect("countdown optimized IR")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));

        let (result, _, _) = execute(&output.code, &[], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(15));
    }

    #[test]
    fn executes_register_bitwise_loop_through_machine_ir_backend() {
        let view = bitwise_loop_view();
        let hir = NumericFunction::build(&view).expect("bitwise-loop numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerAnd(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerOr(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerXor(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerShiftLeft(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerShiftRight(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerNot(..)))
        );

        let sequence = select(&hir).expect("bitwise-loop Machine IR");
        sequence
            .allocate(&TargetSpec::aarch64())
            .expect("bitwise-loop Machine IR allocation");

        let output = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7005,
            &TransitionTable::resolve(),
            false,
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "bitwiseKernel".to_string(),
                    module: "benchmarks/scripts/bitwise-mix.js".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
        )
        .expect("production optimizing selector compiles bitwise loop");
        let optimized_ir = std::str::from_utf8(
            output
                .artifact
                .as_ref()
                .expect("bitwise-loop artifact")
                .file(JitArtifactFileName::OptimizedIr)
                .expect("bitwise-loop optimized IR")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));

        let (result, _, _) = execute(&output.code, &[], 0);
        let mut expected = 0x1234_5678_i32;
        for index in 0_i32..35 {
            let left = expected.wrapping_shl(index as u32 & 31);
            let right = expected >> 31;
            expected = !(((left ^ right) | index) & i32::MAX);
        }
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(expected));
    }

    #[test]
    fn float64_bitwise_inputs_use_exact_to_int32_semantics() {
        for (value, expected) in [
            (0.0, 0),
            (-0.0, 0),
            (1.9, 1),
            (-1.9, -1),
            (f64::NAN, 0),
            (f64::INFINITY, 0),
            (f64::NEG_INFINITY, 0),
            (2_147_483_648.0, i32::MIN),
            (4_294_967_295.0, -1),
            (4_294_967_297.0, 1),
            (-4_294_967_297.0, -1),
            (9_007_199_254_740_992.0, 0),
        ] {
            let code = compile_output(&float_bitwise_view(Op::BitwiseOr), None).code;
            let (result, _, _) = execute(&code, &[boxed_f64(value), tag::box_int32(0)], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(compiled_payload_bits(result), tag::box_int32(expected));
        }

        let code = compile_output(&float_bitwise_view(Op::Ushr), None).code;
        let (result, _, _) = execute(&code, &[boxed_f64(-1.9), tag::box_int32(0)], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(unbox_number(compiled_payload_bits(result)), 4_294_967_295.0);

        let code = compile_output(&float_bitwise_view(Op::Shl), None).code;
        let (result, _, _) = execute(&code, &[boxed_f64(1.9), boxed_f64(33.9)], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(2));

        let view = boolean_bitwise_view();
        let hir = NumericFunction::build(&view).expect("Boolean constants numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::BooleanToInt32(..)))
        );
        let (result, _, _) = execute(&compile_output(&view, None).code, &[], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(1));
    }

    #[test]
    fn checked_integer_subtraction_reconstructs_pre_operation_frames() {
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let register = compile_output(&checked_binary_view(Op::Sub, i32::MIN, 1), None).code;
        let (result, frame, pc) =
            execute_with_poll_cells(&register, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 2);
        assert_eq!(
            frame,
            [
                tag::box_int32(i32::MIN),
                tag::box_int32(1),
                Value::undefined().to_bits()
            ]
        );

        let immediate =
            compile_output(&checked_immediate_view(Op::SubImm, i32::MAX, -1), None).code;
        let (result, frame, pc) =
            execute_with_poll_cells(&immediate, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 1);
        assert_eq!(
            frame,
            [tag::box_int32(i32::MAX), Value::undefined().to_bits()]
        );

        let increment =
            compile_output(&checked_immediate_view(Op::Increment, i32::MAX, 1), None).code;
        let (result, frame, pc) =
            execute_with_poll_cells(&increment, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 1);
        assert_eq!(
            frame,
            [tag::box_int32(i32::MAX), Value::undefined().to_bits()]
        );
    }

    #[test]
    fn checked_integer_multiply_deopts_on_overflow_and_negative_zero() {
        let success = compile_output(&checked_binary_view(Op::Mul, 12_345, -17), None).code;
        let (result, _, _) = execute(&success, &[], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(-209_865));

        let interrupt = 0_u8;
        for (left, right) in [(i32::MAX, 2), (0, -1)] {
            let code = compile_output(&checked_binary_view(Op::Mul, left, right), None).code;
            let mut fuel = i64::MAX as u64;
            let (result, frame, pc) =
                execute_with_poll_cells(&code, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::SideExit)
            );
            assert_eq!(pc, 2);
            assert_eq!(
                frame,
                [
                    tag::box_int32(left),
                    tag::box_int32(right),
                    Value::undefined().to_bits()
                ]
            );
        }
    }

    #[test]
    fn checked_integer_negation_deopts_on_overflow_and_negative_zero() {
        let success = compile_output(&checked_neg_view(17), None).code;
        let (result, _, _) = execute(&success, &[], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(-17));

        let interrupt = 0_u8;
        for source in [0, i32::MIN] {
            let code = compile_output(&checked_neg_view(source), None).code;
            let mut fuel = i64::MAX as u64;
            let (result, frame, pc) =
                execute_with_poll_cells(&code, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::SideExit)
            );
            assert_eq!(pc, 1);
            assert_eq!(
                frame,
                [tag::box_int32(source), Value::undefined().to_bits()]
            );
        }
    }

    #[test]
    fn float_leaf_math_and_truthiness_preserve_javascript_edges() {
        let rem_view = float_binary_view(Op::Rem);
        let rem_hir = NumericFunction::build(&rem_view).expect("remainder numeric HIR");
        let rem_sequence = select(&rem_hir).expect("remainder Machine IR");
        rem_sequence
            .allocate(&TargetSpec::aarch64())
            .expect("remainder allocation");

        for (op, left, right, expected) in [
            (Op::Rem, 5.5, 2.0, 1.5),
            (Op::Pow, 2.0, 10.0, 1024.0),
            (Op::Pow, f64::NAN, 0.0, 1.0),
        ] {
            let code = compile_output(&float_binary_view(op), None).code;
            let (result, _, _) = execute(&code, &[boxed_f64(left), boxed_f64(right)], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(unbox_number(compiled_payload_bits(result)), expected);
        }

        let rem = compile_output(&float_binary_view(Op::Rem), None).code;
        let (result, _, _) = execute(&rem, &[boxed_f64(-4.0), boxed_f64(2.0)], 0);
        assert_eq!(
            unbox_number(compiled_payload_bits(result)).to_bits(),
            (-0.0_f64).to_bits()
        );

        let pow = compile_output(&float_binary_view(Op::Pow), None).code;
        let (result, _, _) = execute(&pow, &[boxed_f64(-1.0), boxed_f64(f64::INFINITY)], 0);
        assert!(unbox_number(compiled_payload_bits(result)).is_nan());

        let truthiness = compile_output(&float_truthiness_view(), None).code;
        for (input, expected) in [(0.0, true), (-0.0, true), (f64::NAN, true), (3.5, false)] {
            let (result, _, _) = execute(&truthiness, &[boxed_f64(input)], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(
                compiled_payload_bits(result),
                Value::boolean(expected).to_bits()
            );
        }

        for (input, expected) in [(0, true), (-1, false)] {
            let code = compile_output(&integer_truthiness_view(input), None).code;
            let (result, _, _) = execute(&code, &[], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(
                compiled_payload_bits(result),
                Value::boolean(expected).to_bits()
            );
        }
    }

    #[test]
    fn unsigned_shift_boxes_and_deopts_as_uint32() {
        for (left, shift, expected) in [
            (-1, 0, 4_294_967_295_f64),
            (-1, 1, 2_147_483_647_f64),
            (i32::MIN, -1, 1_f64),
        ] {
            let code = compile_output(&ushr_view(left, shift), None).code;
            let (result, _, _) = execute(&code, &[], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(unbox_number(compiled_payload_bits(result)), expected);
        }

        let comparison = compile_output(&ushr_comparison_view(), None).code;
        let (result, _, _) = execute(&comparison, &[], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(
            compiled_payload_bits(result),
            Value::boolean(true).to_bits()
        );

        let view = ushr_backedge_view();
        let hir = NumericFunction::build(&view).expect("uint32-loop numeric HIR");
        let poll_state = hir
            .frame_states
            .iter()
            .find(|state| matches!(state.point, NumericFramePoint::Backedge { .. }))
            .expect("uint32 backedge FrameState");
        let uint_value = match poll_state.frames[0].slots[0] {
            hir::NumericFrameSlot::Value(value) => value,
            hir::NumericFrameSlot::Undefined => panic!("uint32 loop value is live"),
        };
        assert_eq!(hir.nodes[uint_value.0].value_type(), NumericType::Uint32);

        let code = compile_output(&view, None).code;
        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) =
            execute_with_poll_cells(&code, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 6);
        assert_eq!(
            frame,
            [
                boxed_f64(4_294_967_295_f64),
                tag::box_int32(POLL_BATCH),
                Value::undefined().to_bits(),
                tag::box_int32(0),
                Value::undefined().to_bits(),
                Value::undefined().to_bits(),
                Value::undefined().to_bits(),
                Value::undefined().to_bits(),
            ],
            "the Uint32 exit must retain the exact state before batched phi edits"
        );
    }

    #[test]
    fn callee_saved_value_survives_leaf_call_and_overflow_deopt() {
        let view = typed_parameter_leaf_overflow_view();
        let hir = NumericFunction::build(&view).expect("typed leaf overflow numeric HIR");
        let sequence = select(&hir).expect("typed leaf overflow Machine IR");
        let allocation = sequence
            .allocate(&TargetSpec::aarch64())
            .expect("typed leaf overflow allocation");
        assert!(allocation.metadata().iter().any(|metadata| {
            metadata.frame_state.is_some()
                && matches!(metadata.location, AllocatedLocation::Register(register)
                    if (register.is_integer() && (20..=28).contains(&register.encoding()))
                        || (register.is_float() && (8..=15).contains(&register.encoding())))
        }));

        let code = compile_output(&view, None).code;
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) = execute_with_poll_cells(
            &code,
            &[tag::box_int32(i32::MAX)],
            0,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 5);
        assert_eq!(frame[0], tag::box_int32(i32::MAX));
        assert_eq!(frame[5], tag::box_int32(1));
    }

    #[test]
    fn register_comparisons_return_canonical_booleans() {
        for (op, left, right, expected) in [
            (Op::Equal, 7, 7, true),
            (Op::NotEqual, 7, 8, true),
            (Op::LessThan, -1, 0, true),
            (Op::LessEq, 7, 7, true),
            (Op::GreaterThan, 8, 7, true),
            (Op::GreaterEq, 7, 7, true),
        ] {
            let code = compile_output(&integer_comparison_view(op, left, right), None).code;
            let (result, _, _) = execute(&code, &[], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(
                compiled_payload_bits(result),
                Value::boolean(expected).to_bits()
            );
        }

        for (op, left, right, expected) in [
            (Op::Equal, 3.5, 3.5, true),
            (Op::NotEqual, f64::NAN, f64::NAN, true),
            (Op::LessThan, f64::NAN, 1.0, false),
            (Op::LessEq, f64::NAN, 1.0, false),
            (Op::GreaterThan, f64::NAN, 1.0, false),
            (Op::GreaterEq, f64::NAN, 1.0, false),
        ] {
            let code = compile_output(&float_comparison_view(op), None).code;
            let (result, _, _) = execute(&code, &[boxed_f64(left), boxed_f64(right)], 0);
            assert_eq!(
                result.validate(NativeResultDomain::Compiled),
                Some(NativeResultStatus::Success)
            );
            assert_eq!(
                compiled_payload_bits(result),
                Value::boolean(expected).to_bits()
            );
        }
    }

    #[test]
    fn publishes_combined_integer_scalar_loop_through_machine_ir_backend() {
        let view = integer_scalar_loop_view();
        let hir = NumericFunction::build(&view).expect("integer-scalar numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerMul(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerShiftRightLogical(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerLessThan(..)))
        );
        let sequence = select(&hir).expect("integer-scalar Machine IR");
        let allocation = sequence
            .allocate(&TargetSpec::aarch64())
            .expect("integer-scalar allocation");
        let frame = arm64::frame_layout(&allocation, 0).expect("integer-scalar frame");
        lower_deopt_table(
            &sequence,
            &allocation,
            frame,
            arm64::GPR_BUDGET,
            arm64::FP_BUDGET,
            &machine_frame_states(&hir),
        )
        .expect("integer-scalar deopt table");

        let output = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7006,
            &TransitionTable::resolve(),
            false,
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "engineKernel".to_string(),
                    module: "benchmarks/scripts/integer-scalar.js".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
        )
        .expect("production selector compiles integer-scalar loop");
        let optimized_ir = std::str::from_utf8(
            output
                .artifact
                .as_ref()
                .expect("integer-scalar artifact")
                .file(JitArtifactFileName::OptimizedIr)
                .expect("integer-scalar optimized IR")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));

        let (result, _, _) = execute(&output.code, &[], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(unbox_number(compiled_payload_bits(result)), 1725.0);
    }

    #[test]
    fn publishes_float_leaf_loop_through_machine_ir_backend() {
        let target = TargetSpec::aarch64();
        let view = float_leaf_loop_view();
        let hir = NumericFunction::build(&view).expect("float-leaf-loop numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::IntegerNeg(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::Rem(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::Pow(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::FloatToBoolean(..)))
        );
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::BooleanNot(..)))
        );

        let sequence = select(&hir).expect("float-leaf-loop Machine IR");
        let leaf = sequence
            .instructions()
            .iter()
            .find(|instruction| instruction.opcode == MachineOpcode::FloatRem)
            .expect("fixed-ABI FP leaf");
        assert_eq!(
            leaf.operands
                .iter()
                .map(|operand| operand.constraint)
                .collect::<Vec<_>>(),
            [
                OperandConstraint::Fixed(target.float_argument(0).expect("float argument 0")),
                OperandConstraint::Fixed(target.float_argument(1).expect("float argument 1")),
                OperandConstraint::Fixed(target.float_result()),
            ]
        );
        let allocation = sequence
            .allocate(&target)
            .expect("float-leaf-loop allocation");
        assert!(
            allocation
                .used_registers()
                .any(|register| target.is_callee_saved(register)),
            "values live across leaf calls must occupy callee-saved registers"
        );
        let output = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7007,
            &TransitionTable::resolve(),
            false,
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "engineKernel".to_string(),
                    module: "benchmarks/scripts/float-leaf-math.js".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
        )
        .expect("production selector compiles float leaf loop");
        let optimized_ir = std::str::from_utf8(
            output
                .artifact
                .as_ref()
                .expect("float-leaf-loop artifact")
                .file(JitArtifactFileName::OptimizedIr)
                .expect("float-leaf-loop optimized IR")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));
        assert!(optimized_ir.contains("FloatRem"));
        assert!(optimized_ir.contains("FloatPow"));
        assert!(!optimized_ir.contains("FloatLeafResult"));

        let (result, _, _) = execute(&output.code, &[], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(199_999));

        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) = execute_with_poll_cells(
            &output.code,
            &[],
            0,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 6);
        let mut expected = vec![Value::undefined().to_bits(); 19];
        expected[0] = boxed_f64(12.073_463_237_907_212);
        expected[1] = tag::box_int32(POLL_BATCH);
        expected[2] = tag::box_int32(POLL_BATCH + 1);
        expected[3] = tag::box_int32(200_000);
        assert_eq!(
            frame, expected,
            "the FP-leaf exit must publish the exact state before batched phi edits"
        );
    }

    #[test]
    fn publishes_float_bitwise_loop_through_machine_ir_backend() {
        let target = TargetSpec::aarch64();
        let view = float_bitwise_loop_view();
        let hir = NumericFunction::build(&view).expect("float-bitwise-loop numeric HIR");
        assert!(
            hir.nodes
                .iter()
                .filter(|node| matches!(node, NumericNode::FloatToInt32(..)))
                .count()
                >= 2
        );
        let sequence = select(&hir).expect("float-bitwise-loop Machine IR");
        let leaf = sequence
            .instructions()
            .iter()
            .find(|instruction| instruction.opcode == MachineOpcode::Float64ToInt32)
            .expect("fixed-ABI ToInt32 leaf");
        assert_eq!(
            leaf.operands
                .iter()
                .map(|operand| operand.constraint)
                .collect::<Vec<_>>(),
            [
                OperandConstraint::Fixed(target.float_argument(0).expect("float argument 0")),
                OperandConstraint::Fixed(target.integer_result()),
            ]
        );
        let allocation = sequence
            .allocate(&target)
            .expect("float-bitwise-loop allocation");
        assert!(
            allocation
                .used_registers()
                .any(|register| target.is_callee_saved(register)),
            "loop-carried values live across ToInt32 leaves must occupy callee-saved registers"
        );

        let output = crate::optimizing::compile_optimized_with_artifacts(
            &view,
            7008,
            &TransitionTable::resolve(),
            false,
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "engineKernel".to_string(),
                    module: "benchmarks/scripts/float-bitwise.js".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
        )
        .expect("production selector compiles float bitwise loop");
        let optimized_ir = std::str::from_utf8(
            output
                .artifact
                .as_ref()
                .expect("float-bitwise artifact")
                .file(JitArtifactFileName::OptimizedIr)
                .expect("float-bitwise optimized IR")
                .contents(),
        )
        .expect("UTF-8 optimized IR");
        assert!(optimized_ir.starts_with("; backend=otter-machine-ir scalar-function\n"));
        assert!(optimized_ir.contains("Float64ToInt32"));
        assert!(!optimized_ir.contains("IntegerLeafResult"));

        let (result, _, _) = execute(&output.code, &[], 0);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(120_790));

        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) = execute_with_poll_cells(
            &output.code,
            &[],
            0,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 4);
        let mut expected = vec![Value::undefined().to_bits(); 12];
        expected[0] = boxed_f64(4_294_967_321.75);
        expected[1] = tag::box_int32(12);
        expected[2] = tag::box_int32(POLL_BATCH);
        expected[3] = tag::box_int32(200_000);
        assert_eq!(
            frame, expected,
            "the bitwise exit must publish the exact state before batched phi edits"
        );
    }

    #[test]
    fn checked_integer_overflow_reconstructs_exact_mid_loop_frames() {
        let add = compile_output(&branch_phi_loop_view_with(i32::MAX, 0, 1, 1), None).code;
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) =
            execute_with_poll_cells(&add, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 13);
        assert_eq!(frame[0], tag::box_int32(i32::MAX));
        assert_eq!(frame[1], tag::box_int32(0));
        assert_eq!(frame[2], tag::box_int32(2));

        let add_immediate =
            compile_output(&branch_phi_loop_view_with(0, 1, 2, i32::MAX), None).code;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) = execute_with_poll_cells(
            &add_immediate,
            &[],
            0,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 15);
        assert_eq!(frame[0], tag::box_int32(-14));
        assert_eq!(frame[1], tag::box_int32(1));
    }

    #[test]
    fn protected_inner_loop_has_no_osr_entry_while_outer_header_remains_available() {
        let value = hir::NumericValue;
        let block = |logical_pc: u32,
                     osr_entry_allowed: bool,
                     predecessors: Vec<usize>,
                     successors: Vec<usize>,
                     nodes: Vec<hir::NumericValue>,
                     terminator: NumericTerminator| {
            hir::NumericBlock {
                logical_pc,
                osr_entry_allowed,
                predecessors,
                successor_arguments: vec![Vec::new(); successors.len()],
                successors,
                parameters: Vec::new(),
                parameter_registers: Vec::new(),
                nodes,
                terminator,
            }
        };
        let hir = NumericFunction {
            property_sites: BTreeMap::new(),
            constructor_field_sites: BTreeMap::new(),
            function_id: 190,
            nodes: vec![
                NumericNode::TaggedConstant(Value::undefined().to_bits()),
                NumericNode::BooleanConstant(true),
                NumericNode::BooleanConstant(true),
            ],
            blocks: vec![
                block(
                    0,
                    true,
                    Vec::new(),
                    vec![1],
                    vec![value(0)],
                    NumericTerminator::Jump,
                ),
                block(
                    1,
                    true,
                    vec![0, 4],
                    vec![2, 5],
                    vec![value(1)],
                    NumericTerminator::Branch {
                        condition: value(1),
                        when_true: true,
                    },
                ),
                block(
                    2,
                    false,
                    vec![1, 3],
                    vec![3, 4],
                    vec![value(2)],
                    NumericTerminator::Branch {
                        condition: value(2),
                        when_true: true,
                    },
                ),
                block(
                    3,
                    false,
                    vec![2],
                    vec![2],
                    Vec::new(),
                    NumericTerminator::Jump,
                ),
                block(
                    4,
                    true,
                    vec![2],
                    vec![1],
                    Vec::new(),
                    NumericTerminator::Jump,
                ),
                block(
                    5,
                    true,
                    vec![1],
                    Vec::new(),
                    Vec::new(),
                    NumericTerminator::Return(value(0)),
                ),
            ],
            frame_states: vec![
                hir::NumericFrameState {
                    point: NumericFramePoint::Backedge {
                        predecessor: 3,
                        edge: 0,
                    },
                    frames: Box::new([otter_vm::deopt::DeoptFrame {
                        function_id: 190,
                        byte_pc: 16,
                        entry: None,
                        slots: (Vec::new()).into(),
                    }]),
                },
                hir::NumericFrameState {
                    point: NumericFramePoint::Backedge {
                        predecessor: 4,
                        edge: 0,
                    },
                    frames: Box::new([otter_vm::deopt::DeoptFrame {
                        function_id: 190,
                        byte_pc: 8,
                        entry: None,
                        slots: (Vec::new()).into(),
                    }]),
                },
            ],
            direct_call_targets: Vec::new(),
            operand_values: Vec::new(),
            parameter_count: 0,
            register_count: 0,
            arithmetic_op_count: 0,
        };
        let sequence = select(&hir).expect("nested-loop Machine body");
        let osr_pcs = sequence
            .instructions()
            .iter()
            .filter_map(|instruction| match instruction.opcode {
                MachineOpcode::OsrEntry { logical_pc, .. } => Some(logical_pc),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(osr_pcs, [1]);
        sequence
            .verify(&TargetSpec::aarch64())
            .expect("nested-loop Machine verification");
    }

    #[test]
    fn numeric_osr_enters_exact_header_and_rejects_without_vm_mutation() {
        let code = compile_output(&branch_phi_loop_view_with(0, 0, 5, 1), None).code;
        let mut frame = vec![Value::undefined().to_bits(); 12];
        frame[0] = tag::box_int32(2);
        frame[1] = tag::box_int32(1);
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, after, pc) = execute_osr_with_poll_cells(
            &code,
            3,
            frame.clone(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(-22));
        assert_eq!(after, frame, "successful OSR must keep VM slots untouched");
        assert_eq!(pc, 3);

        frame[0] = Value::boolean(true).to_bits();
        let rejected = frame.clone();
        let mut fuel = i64::MAX as u64;
        let (result, after, pc) =
            execute_osr_with_poll_cells(&code, 3, frame, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 3);
        assert_eq!(after, rejected, "OSR representation reject must be atomic");
    }

    #[test]
    fn numeric_osr_deopts_overflow_and_interrupts_before_phi_moves() {
        let overflow = compile_output(&branch_phi_loop_view_with(i32::MAX, 0, 1, 1), None).code;
        let mut frame = vec![Value::undefined().to_bits(); 12];
        frame[0] = tag::box_int32(i32::MAX);
        frame[1] = tag::box_int32(0);
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) = execute_osr_with_poll_cells(
            &overflow,
            3,
            frame,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 13);
        assert_eq!(frame[0], tag::box_int32(i32::MAX));
        assert_eq!(frame[1], tag::box_int32(0));
        assert_eq!(frame[2], tag::box_int32(2));

        let loop_limit = POLL_BATCH + 4;
        let code = compile_output(&branch_phi_loop_view_with(0, 0, loop_limit, 1), None).code;
        let mut frame = vec![Value::undefined().to_bits(); 12];
        frame[0] = tag::box_int32(2);
        frame[1] = tag::box_int32(1);
        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) =
            execute_osr_with_poll_cells(&code, 3, frame, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 3);
        let mut expected = vec![Value::undefined().to_bits(); 12];
        expected[0] = tag::box_int32(-94);
        expected[1] = tag::box_int32(POLL_BATCH + 1);
        assert_eq!(
            frame, expected,
            "the OSR interrupt exit must publish the exact pre-phi state"
        );
    }

    #[test]
    fn numeric_osr_materializes_float_uint32_and_boolean_headers() {
        let float_view = float_bitwise_loop_view();
        let float_hir = NumericFunction::build(&float_view).expect("float OSR HIR");
        let float_sequence = select(&float_hir).expect("float OSR Machine IR");
        let float_inputs = float_sequence
            .instructions()
            .iter()
            .find_map(|instruction| match &instruction.opcode {
                MachineOpcode::OsrEntry {
                    logical_pc: 4,
                    inputs,
                } => Some(inputs.as_slice()),
                _ => None,
            })
            .expect("float OSR marker");
        assert_eq!(
            float_inputs,
            [
                MachineOsrInput {
                    frame_register: 0,
                    value_type: MachineOsrType::Float64,
                },
                MachineOsrInput {
                    frame_register: 1,
                    value_type: MachineOsrType::Int32,
                },
                MachineOsrInput {
                    frame_register: 2,
                    value_type: MachineOsrType::Int32,
                },
                MachineOsrInput {
                    frame_register: 3,
                    value_type: MachineOsrType::Int32,
                },
            ]
        );
        let float_code = compile_output(&float_view, None).code;
        let mut float_frame = vec![Value::undefined().to_bits(); 12];
        float_frame[0] = boxed_f64(4_294_967_299.25);
        float_frame[1] = tag::box_int32(0);
        float_frame[2] = tag::box_int32(1);
        float_frame[3] = tag::box_int32(200_000);
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, after, _) = execute_osr_with_poll_cells(
            &float_code,
            4,
            float_frame.clone(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(120_790));
        assert_eq!(after, float_frame);

        let view = mixed_osr_loop_view();
        let hir = NumericFunction::build(&view).expect("mixed OSR numeric HIR");
        let sequence = select(&hir).expect("mixed OSR Machine IR");
        let inputs = sequence
            .instructions()
            .iter()
            .find_map(|instruction| match &instruction.opcode {
                MachineOpcode::OsrEntry {
                    logical_pc: 6,
                    inputs,
                } => Some(inputs.as_slice()),
                _ => None,
            })
            .expect("mixed OSR marker");
        assert_eq!(
            inputs
                .iter()
                .map(|input| input.value_type)
                .collect::<Vec<_>>(),
            [
                MachineOsrType::Uint32,
                MachineOsrType::Boolean,
                MachineOsrType::Int32,
                MachineOsrType::Int32,
            ]
        );

        let code = compile_output(&view, None).code;
        let mut frame = vec![Value::undefined().to_bits(); 10];
        frame[0] = boxed_f64(f64::from(u32::MAX));
        frame[1] = Value::boolean(true).to_bits();
        frame[2] = tag::box_int32(0);
        frame[3] = tag::box_int32(3);
        let mut fuel = i64::MAX as u64;
        let (result, after, _) = execute_osr_with_poll_cells(
            &code,
            6,
            frame.clone(),
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(536_870_911));
        assert_eq!(after, frame);

        frame[1] = tag::box_int32(1);
        let rejected = frame.clone();
        let mut fuel = i64::MAX as u64;
        let (result, after, pc) =
            execute_osr_with_poll_cells(&code, 6, frame, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 6);
        assert_eq!(after, rejected);
    }

    #[test]
    fn numeric_osr_materializes_allocator_spill_homes() {
        const LIVE_VALUES: usize = 30;
        const HEADER_PC: u32 = LIVE_VALUES as u32 + 3;
        let view = osr_spill_pressure_loop_view();
        let hir = NumericFunction::build(&view).expect("OSR spill-pressure HIR");
        let sequence = select(&hir).expect("OSR spill-pressure Machine IR");
        let marker = sequence
            .instructions()
            .iter()
            .position(|instruction| matches!(instruction.opcode, MachineOpcode::OsrEntry { .. }))
            .map(|index| MachineInstructionId(index as u32))
            .expect("OSR spill-pressure marker");
        let allocation = sequence
            .allocate(&TargetSpec::aarch64())
            .expect("OSR spill-pressure allocation");
        assert!(
            allocation
                .instruction_locations(marker)
                .expect("OSR marker locations")
                .iter()
                .any(|location| matches!(location, AllocatedLocation::Stack(_))),
            "OSR pressure fixture must exercise direct spill materialization"
        );

        let code = compile_output(&view, None).code;
        let mut frame = vec![Value::undefined().to_bits(); LIVE_VALUES + 5];
        for (register, slot) in frame.iter_mut().enumerate().take(LIVE_VALUES) {
            *slot = tag::box_int32(register as i32 + 1);
        }
        frame[LIVE_VALUES] = tag::box_int32(0);
        frame[LIVE_VALUES + 1] = tag::box_int32(1);
        frame[LIVE_VALUES + 3] = tag::box_int32(0);
        let original = frame.clone();
        let interrupt = 0_u8;
        let mut fuel = i64::MAX as u64;
        let (result, after, _) = execute_osr_with_poll_cells(
            &code,
            HEADER_PC,
            frame,
            std::ptr::addr_of!(interrupt),
            &mut fuel,
        );
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(465));
        assert_eq!(after, original);
    }

    #[test]
    fn backedge_poll_refills_fuel_and_interrupt_bails_before_phi_moves() {
        extern "C" fn refill(ctx: *mut JitCtx) -> u64 {
            // SAFETY: the execution fixture keeps its thread and fuel cell live.
            unsafe {
                let thread = &*(*ctx).thread;
                *(thread.backedge_fuel_cell as *mut u64) = 100;
            }
            NativeResultStatus::Success as u64
        }

        let mut transitions = TransitionTable::resolve();
        transitions.replace_entry_for_test(STUB_JIT_BACKEDGE_POLL, refill as *const () as usize);
        let loop_limit = POLL_BATCH * 2 + 1;
        let code = compile_output_with_transitions(
            &branch_phi_loop_view_with(0, 0, loop_limit, 1),
            &transitions,
            None,
        )
        .code;
        let interrupt = 0_u8;
        let mut fuel = 1_u64;
        let (result, _, _) =
            execute_with_poll_cells(&code, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::Success)
        );
        assert_eq!(compiled_payload_bits(result), tag::box_int32(-190));
        assert_eq!(fuel, 84);

        let interrupt = 1_u8;
        let mut fuel = i64::MAX as u64;
        let (result, frame, pc) =
            execute_with_poll_cells(&code, &[], 0, std::ptr::addr_of!(interrupt), &mut fuel);
        assert_eq!(
            result.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 3);
        let mut expected = vec![Value::undefined().to_bits(); 12];
        expected[0] = tag::box_int32(-96);
        expected[1] = tag::box_int32(POLL_BATCH);
        assert_eq!(
            frame, expected,
            "the batched interrupt exit must precede loop-header phi edits"
        );
    }

    #[test]
    fn number_guard_bails_before_observable_effects() {
        let code = compile_output(&identity_view(), None).code;
        let input = Value::undefined().to_bits();
        let (ret, frame, pc) = execute(&code, &[input], 91);

        assert_eq!(
            ret.validate(NativeResultDomain::Compiled),
            Some(NativeResultStatus::SideExit)
        );
        assert_eq!(pc, 0);
        assert_eq!(frame[0], input);
    }

    #[test]
    fn artifact_identifies_the_installed_machine_ir_code_object() {
        let output = compile_output(
            &identity_view(),
            Some(ArtifactRequest {
                identity: JitArtifactIdentity {
                    function_name: "numericMachineLeaf".to_string(),
                    module: "test:numeric-machine-leaf".to_string(),
                },
                tier: JitDebugTier::Optimizing,
                entry: JitDebugTarget::Entry,
            }),
        );
        let artifact = output.artifact.expect("requested artifact bundle");
        let text = |name| {
            std::str::from_utf8(artifact.file(name).expect("artifact payload").contents())
                .expect("text artifact")
        };

        assert!(
            text(JitArtifactFileName::OptimizedIr)
                .starts_with("; backend=otter-machine-ir scalar-function\n")
        );
        assert!(text(JitArtifactFileName::CodeMap).contains("\"kind\": \"machineScalarFunction\""));
        assert_eq!(
            artifact
                .file(JitArtifactFileName::Code)
                .expect("exact code artifact")
                .contents(),
            output.code.compiled_code().bytes(),
        );
    }
}
