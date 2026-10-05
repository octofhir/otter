//! The graph tier: the optimizing compiler that builds an SSA graph straight
//! from bytecode and feedback, plans canonical spill homes over live intervals, and
//! emits machine code on those registers.
//!
//! # Contents
//! - [`bytecode`] — blocks, loops and register liveness of the bytecode.
//! - [`ir`] — the node vocabulary and graph arena.
//! - [`builder`] — the abstract interpreter turning bytecode and feedback
//!   into the graph.
//! - [`feedback`] — the snapshot's per-site feedback as speculation.
//! - [`phi_repr`] — unboxed int32 phis.
//! - [`truncation`] — wrapping int32 additions under truncating uses.
//! - [`licm`] — loop-invariant checks and loads run once before their loop.
//! - [`regalloc`] — live intervals, canonical homes and register assignment.
//! - [`registers`] — backend-supplied physical ownership and fixed call words.
//! - [`frame`] / [`emission`] — shared canonical geometry and finalized output.
//! - [`liveness`] — control-flow liveness of SSA and frame-state values.
//! - [`metadata`] — shared source attribution, safepoint and property-cell plans.
//! - [`call`] — typed operands of committed collecting runtime calls.
//! - [`native_leaf`] — pure explicit-receiver declaration admission.
//! - [`dump`] — complete constant declarations and block listings for artifacts.
//! - [`moves`] — parallel edge and exit assignments for each target encoder.
//! - `arm64` / `x86_64` — target-native instruction encoders.
//! - [`compile`] — the pipeline from snapshot to code bytes.
//! - Captured inline diagnostics describe the canonical bodies actually
//!   spliced into the completed graph, including nested source attribution.
//!
//! # Invariants
//! - An instruction without a specialized form runs through its baseline
//!   operation on the frame window, and one that cannot run in optimized
//!   code leaves through an unconditional deopt. Optional compilation can
//!   decline a frame that exceeds the native metadata's representation.
//! - Speculation always branches straight to an eager deopt exit that
//!   rebuilds the interpreter frame before the failed instruction.
//! - Backend contracts exclude pinned and emitter scratch registers from
//!   allocation. Nodes declare every other register they need.
//!
//! # See also
//! - `otter_vm::deopt` — the frame-state recipe the exits produce.
//! - [`crate::template`] — the baseline tier whose operations generic nodes
//!   reuse.

mod allocation_groups;
#[cfg(target_arch = "aarch64")]
pub(crate) mod arm64;
pub(crate) mod builder;
pub(crate) mod bytecode;
pub(crate) mod call;
mod dump;
pub(crate) mod emission;
pub(crate) mod feedback;
pub(crate) mod frame;
pub(crate) mod ir;
pub(crate) mod licm;
pub(crate) mod liveness;
pub(crate) mod metadata;
pub(crate) mod moves;
mod native_leaf;
pub(crate) mod phi_repr;
pub(crate) mod regalloc;
pub(crate) mod registers;
pub(crate) mod truncation;
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;

use otter_vm::JitCompileSnapshot;
use otter_vm::deopt::{DeoptExitDescriptor, DeoptRuntime, DeoptTable, FrameState};

use crate::optimizing::{OptimizedCode, OptimizedMetadata};
use crate::{CompiledCode, Unsupported};

/// Bounded inline prototype traversal before committed runtime completion.
const INSTANCEOF_CHAIN_BOUND: u32 = 32;

/// Per-PC facts the builder takes from the baseline tier's plan.
#[derive(Debug, Clone)]
pub(crate) struct BaselineSupport {
    /// `true` when the baseline tier emits the instruction at this PC.
    pub(crate) supported: Vec<bool>,
}

impl BaselineSupport {
    /// Whether the instruction at `pc` has a baseline operation.
    pub(crate) fn has_operation(&self, pc: u32) -> bool {
        self.supported.get(pc as usize).copied().unwrap_or(false)
    }

    pub(crate) fn of(plan: &crate::template::TemplatePlan, view: &JitCompileSnapshot) -> Self {
        let mut supported = vec![true; view.instructions.len()];
        for instruction in &plan.instructions {
            if matches!(instruction.op, crate::template::TemplateOp::UnsupportedBail)
                && let Some(slot) = supported.get_mut(instruction.pc as usize)
            {
                *slot = false;
            }
        }
        Self { supported }
    }
}

/// The compiled bytes and the metadata built with them.
pub(crate) struct Compiled {
    pub(crate) emission: emission::Emission,
    pub(crate) built: builder::Built,
    pub(crate) allocation: regalloc::Allocation,
    /// Exit recipes; their address is baked into the code.
    pub(crate) deopt: Box<DeoptRuntime>,
    /// The baseline plan generic nodes run; its operand buffers are baked
    /// into the code and live as long as it.
    pub(crate) plan: crate::template::TemplatePlan,
}

/// Compile `view` to machine code; `capture` keeps the relocation records an
/// artifact renders.
pub(crate) fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
    osr_pc: Option<u32>,
    capture: bool,
) -> Result<Compiled, Unsupported> {
    let analysis = std::rc::Rc::new(
        bytecode::Analysis::build(view)
            .map_err(|_| Unsupported::OperandShape("graph bytecode analysis"))?,
    );
    let plan = crate::template::TemplatePlan::build_unfused(view)?;
    let baseline = BaselineSupport::of(&plan, view);
    let mut built = builder::build(view, &analysis, &baseline, osr_pc)
        .map_err(|_| Unsupported::OperandShape("graph construction"))?;
    phi_repr::untag_phis(&mut built.graph, &built.layout, &built.loop_headers);
    // An untagged loop phi whose back edge carried only its own retagged
    // value merges one entry value; drop it before liveness sees it.
    builder::remove_trivial_phis(&mut built.graph, &built.layout);
    for header in &mut built.loop_headers {
        let phis = &built.graph.block(header.block).phis;
        header.phis.retain(|(_, phi)| phis.contains(phi));
    }
    truncation::wrap_truncated_arithmetic(&mut built.graph, &built.layout);
    licm::hoist_invariants(&mut built.graph, &mut built.layout, &built.loop_headers);
    allocation_groups::fold(&mut built.graph, &built.layout, view, &built.inline_views);
    #[cfg(target_arch = "aarch64")]
    let registers = registers::AARCH64;
    #[cfg(target_arch = "x86_64")]
    let registers = registers::X86_64;
    let allocation = regalloc::allocate(&built.graph, &built.layout, registers);
    // The recipe's address is baked before its contents exist: the exits are
    // known only once the code is emitted, and the box never moves.
    let mut deopt = Box::new(DeoptRuntime::default());
    let slots = frame::SlotLayout::of(&allocation)?;
    // Near branches are one instruction; only a body whose own control flow
    // spans more than a conditional branch reaches pays for the far form.
    let deopt_address = std::ptr::from_ref::<DeoptRuntime>(&deopt);
    #[cfg(target_arch = "aarch64")]
    let emit = |far| {
        arm64::emit(
            view,
            &built,
            &allocation,
            transitions,
            code_object_id,
            deopt_address,
            &plan,
            slots,
            capture,
            far,
        )
    };
    #[cfg(target_arch = "aarch64")]
    let mut emission = match emit(false) {
        Err(Unsupported::Backend(crate::BackendFailure::Relocation)) => emit(true),
        result => result,
    }?;
    #[cfg(target_arch = "x86_64")]
    let mut emission = x86_64::emit(
        view,
        &built,
        &allocation,
        transitions,
        code_object_id,
        deopt_address,
        &plan,
        slots,
        capture,
    )?;
    let (runtime, recipe_records) = deopt_runtime(
        &built,
        &allocation,
        slots,
        &emission.exits,
        emission.site_records.len(),
    )?;
    *deopt = runtime;
    emission.site_records.extend(recipe_records);
    Ok(Compiled {
        emission,
        built,
        allocation,
        deopt,
        plan,
    })
}

/// One frame-state recipe per exit site, in exit order, and the safepoint
/// record of each deduplicated recipe, numbered below the `site_records`.
fn deopt_runtime(
    built: &builder::Built,
    allocation: &regalloc::Allocation,
    slots: frame::SlotLayout,
    exits: &[emission::ExitSite],
    site_records: usize,
) -> Result<(DeoptRuntime, Vec<otter_vm::native_abi::SafepointRecord>), Unsupported> {
    let base = metadata::FIRST_SITE_SAFEPOINT
        .checked_sub(u32::try_from(site_records).map_err(|_| {
            Unsupported::OperandShape("graph safepoint records exceed their id space")
        })?)
        .ok_or(Unsupported::OperandShape(
            "graph safepoint records exceed their id space",
        ))?;
    let mut records = Vec::new();
    // Exits that rebuild the same frame share one recipe: several exits of
    // one node, and nodes of one instruction whose values sit in the same
    // locations.
    let mut states = Vec::new();
    let mut recipes: rustc_hash::FxHashMap<FrameState, u32> = rustc_hash::FxHashMap::default();
    let mut descriptors = Vec::with_capacity(exits.len());
    for site in exits {
        let node = built.graph.node(site.node);
        let state_id = if site.lazy { node.lazy } else { node.eager }
            .expect("an exit site has its frame state");
        let node_allocation = allocation.node(site.node);
        let locations: &[regalloc::Location] = if site.lazy {
            &node_allocation.lazy
        } else {
            &node_allocation.eager
        };
        let recipe = FrameState {
            frames: frame::deopt_frames(&built.graph, slots, state_id, locations),
            virtual_objects: Box::new([]),
        };
        let index = *recipes.entry(recipe).or_insert_with_key(|recipe| {
            states.push(recipe.clone());
            (states.len() - 1) as u32
        });
        let safepoint = base.checked_sub(index).ok_or(Unsupported::OperandShape(
            "graph safepoint records exceed their id space",
        ))?;
        if records.len() == index as usize {
            records.push(metadata::recipe_record(
                safepoint,
                locations,
                slots.spill_tagged,
            ));
        }
        descriptors.push(DeoptExitDescriptor {
            state: index,
            reason: site.reason,
            action: site.action,
            resume_pcs: built
                .graph
                .state_chain(state_id)
                .iter()
                .map(|&state| built.graph.frame_state(state).pc)
                .collect(),
            safepoint,
        });
    }
    Ok((
        DeoptRuntime {
            table: DeoptTable::from_states(states),
            exits: descriptors.into_boxed_slice(),
        },
        records,
    ))
}

/// Compile `view` into an installable optimized code object, with its
/// artifact bundle when one is requested.
pub(crate) fn compile_optimized(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
    osr_pc: Option<u32>,
    artifact_request: Option<crate::artifact::ArtifactRequest>,
    capture_events: bool,
) -> Result<crate::artifact::NativeCompileOutput<OptimizedCode>, Unsupported> {
    let compiled = compile(
        view,
        code_object_id,
        transitions,
        osr_pc,
        artifact_request.is_some(),
    )?;
    let Compiled {
        emission,
        built,
        allocation,
        deopt,
        plan,
        ..
    } = compiled;
    let mut safepoints = plan.safepoint_records.clone();
    safepoints.extend(emission.site_records.iter().cloned());
    safepoints.sort_by_key(|record| record.id);
    let metadata = OptimizedMetadata {
        code_object_id,
        function_id: view.code_block.id,
        param_count: view.code_block.param_count,
        register_count: view.code_block.register_count,
        allocator_spill_slot_count: allocation.tagged_slots + allocation.untagged_slots,
        spill_slot_count: allocation.tagged_slots + allocation.untagged_slots,
    };
    let code = CompiledCode::new(
        emission.buffer,
        dynasmrt::AssemblyOffset(emission.tier_entry),
    );
    let node_count = built.graph.nodes.len() as u64;
    let diagnostics = if capture_events {
        inline_diagnostics(view.code_block.id, &built)
    } else {
        Box::default()
    };
    let spliced_functions = built
        .graph
        .inlined
        .iter()
        .map(|body| body.function_id)
        .chain(emission.spliced_functions.iter().copied())
        .filter(|&fid| fid != view.code_block.id)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let artifact = artifact_request.map(|request| {
        let mut code_map = crate::artifact::CodeMapCapture::default();
        // Both encoders return the actual canonical call-entry assembler offset.
        code_map.record_call_entry(emission.call_entry);
        let offsets = &emission.node_offsets;
        for (index, &(start, node)) in offsets.iter().enumerate() {
            let end = offsets
                .get(index + 1)
                .map_or(emission.body_end, |&(next, _)| next);
            let data = built.graph.node(node);
            let source = metadata::source_view(view, &built.inline_views, &built.graph, node);
            let byte_pc = source
                .instructions
                .get(data.pc as usize)
                .map_or(0, |metadata| metadata.byte_pc);
            code_map.record(crate::artifact::CodeRegion::instruction(
                start,
                end,
                data.block.map(|block| block.0),
                None,
                source.code_block.id,
                data.pc,
                byte_pc,
                Some(node.0),
                format!("v{} {:?}", node.0, data.kind),
            ));
        }
        code_map.record(crate::artifact::CodeRegion::structural(
            "out-of-line",
            emission.body_end,
            code.len(),
        ));
        if let (Some(pc), Some(end)) = (osr_pc, emission.osr_dispatch_end) {
            code_map.record_osr(pc, emission.tier_entry, end);
        }
        crate::artifact::build_bundle(
            request,
            view,
            code_object_id,
            &code,
            otter_vm::JitArtifactFileName::OptimizedIr,
            built.graph.dump(&built.layout),
            code_map,
            emission.relocations,
            Some(&deopt),
            &safepoints,
            &emission.return_sites,
        )
    });
    Ok(crate::artifact::NativeCompileOutput {
        code: OptimizedCode::new(
            code,
            emission.call_entry,
            deopt,
            safepoints.into_boxed_slice(),
            emission.return_sites.into_boxed_slice(),
            osr_pc.into_iter().collect(),
            Box::new([]),
            spliced_functions,
            emission.load_ic_cells,
            emission.store_ic_cells,
            plan.register_operands,
            metadata,
        ),
        artifact,
        diagnostics,
        ir_node_count: node_count,
    })
}

/// Accepted splices come from the finished graph, after speculative builder
/// rollbacks and loop rebuilding. The origin chain identifies source parents;
/// the matching snapshot supplies the exact bytecode-byte budget charge.
/// Bake refusals remain VM events; this reports no guessed backend rejection.
fn inline_diagnostics(
    root_function_id: u32,
    built: &builder::Built,
) -> Box<[otter_vm::JitCompilerDiagnostic]> {
    assert_eq!(built.graph.inlined.len(), built.inline_views.len());
    let mut depths = Vec::with_capacity(built.graph.inlined.len());
    let mut diagnostics = Vec::with_capacity(built.graph.inlined.len());
    for (index, (body, view)) in built
        .graph
        .inlined
        .iter()
        .zip(&built.inline_views)
        .enumerate()
    {
        let (parent_function_id, depth) = if body.parent == 0 {
            (root_function_id, 1)
        } else {
            let parent = usize::from(body.parent - 1);
            assert!(parent < index, "an inline parent precedes its body");
            (built.graph.inlined[parent].function_id, depths[parent] + 1)
        };
        depths.push(depth);
        diagnostics.push(otter_vm::JitCompilerDiagnostic::InlineLowered {
            parent_function_id,
            instruction_pc: body.call_pc,
            byte_pc: body.call_byte_pc,
            callee_function_id: body.function_id,
            depth,
            cost: view.code_block.bytecode_byte_len(),
            outcome: otter_vm::JitInlineLoweringOutcome::Inlined,
        });
    }
    diagnostics.into_boxed_slice()
}

#[cfg(test)]
mod source_map_tests;
#[cfg(test)]
mod tests;
