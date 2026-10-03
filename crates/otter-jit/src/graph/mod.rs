//! The graph tier: the optimizing compiler that builds an SSA graph straight
//! from bytecode and feedback, allocates registers in one linear pass, and
//! emits machine code on those registers.
//!
//! # Contents
//! - [`bytecode`] — blocks, loops and register liveness of the bytecode.
//! - [`ir`] — the node vocabulary and graph arena.
//! - [`builder`] — the abstract interpreter turning bytecode and feedback
//!   into the graph.
//! - [`feedback`] — the snapshot's per-site feedback as speculation.
//! - [`phi_repr`] — unboxed int32 phis.
//! - [`regalloc`] — straight-forward linear register allocation.
//! - [`arm64`] — AArch64 code generation.
//! - [`compile`] — the pipeline from snapshot to code bytes.
//!
//! # Invariants
//! - Every function the VM asks for compiles: an instruction without a
//!   specialized form runs through its baseline operation on the frame
//!   window, and one that cannot run in optimized code at all leaves through
//!   an unconditional deopt. No function is declined as a whole.
//! - Speculation always branches straight to an eager deopt exit that
//!   rebuilds the interpreter frame before the failed instruction.
//! - The only machine scratch is `x16`/`x17`/`d31`; nodes declare every
//!   other register they need.
//!
//! # See also
//! - `otter_vm::deopt` — the frame-state recipe the exits produce.
//! - [`crate::template`] — the baseline tier whose operations generic nodes
//!   reuse.

pub(crate) mod arm64;
pub(crate) mod builder;
pub(crate) mod bytecode;
pub(crate) mod feedback;
pub(crate) mod ir;
pub(crate) mod phi_repr;
pub(crate) mod regalloc;

use otter_vm::JitCompileSnapshot;
use otter_vm::deopt::{DeoptExitDescriptor, DeoptRuntime, DeoptTable, FrameState};

use crate::optimizing::{OptimizedCode, OptimizedMetadata};
use crate::{CompiledCode, Unsupported};

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
    pub(crate) emission: arm64::Emission,
    pub(crate) built: builder::Built,
    pub(crate) allocation: regalloc::Allocation,
    /// Exit recipes; their address is baked into the code.
    pub(crate) deopt: Box<DeoptRuntime>,
    pub(crate) slots: arm64::SlotLayout,
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
    let allocation = regalloc::allocate(&built.graph, &built.layout);
    // The recipe's address is baked before its contents exist: the exits are
    // known only once the code is emitted, and the box never moves.
    let mut deopt = Box::new(DeoptRuntime::default());
    let slots = arm64::SlotLayout::of(&allocation);
    // Near branches are one instruction; only a body whose own control flow
    // spans more than a conditional branch reaches pays for the far form.
    let deopt_address = std::ptr::from_ref::<DeoptRuntime>(&deopt);
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
    let emission = match emit(false) {
        Err(Unsupported::Backend(crate::BackendFailure::Relocation)) => emit(true),
        result => result,
    }?;
    *deopt = deopt_runtime(&built, &allocation, slots, &emission.exits);
    Ok(Compiled {
        emission,
        built,
        allocation,
        deopt,
        slots,
        plan,
    })
}

/// One frame-state recipe per exit site, in exit order.
fn deopt_runtime(
    built: &builder::Built,
    allocation: &regalloc::Allocation,
    slots: arm64::SlotLayout,
    exits: &[arm64::ExitSite],
) -> DeoptRuntime {
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
        let locations: &[regalloc::Location] = match &site.locations {
            Some(locations) => locations,
            None if site.lazy => &node_allocation.lazy,
            None => &node_allocation.eager,
        };
        let recipe = FrameState {
            frames: arm64::deopt_frames(&built.graph, slots, state_id, locations),
            virtual_objects: Box::new([]),
        };
        let index = *recipes.entry(recipe).or_insert_with_key(|recipe| {
            states.push(recipe.clone());
            (states.len() - 1) as u32
        });
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
        });
    }
    DeoptRuntime {
        table: DeoptTable::from_states(states),
        exits: descriptors.into_boxed_slice(),
        gpr_budget: regalloc::GP_REGISTERS.len() as u16,
    }
}

/// Compile `view` into an installable optimized code object, with its
/// artifact bundle when one is requested.
pub(crate) fn compile_optimized(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
    osr_pc: Option<u32>,
    artifact_request: Option<crate::artifact::ArtifactRequest>,
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
        machine_register_count: (regalloc::GP_REGISTERS.len() + regalloc::FP_REGISTERS.len()) as u8,
        allocator_spill_slot_count: allocation.tagged_slots + allocation.untagged_slots,
        spill_slot_count: allocation.tagged_slots + allocation.untagged_slots,
    };
    let code = CompiledCode::new(
        emission.buffer,
        dynasmrt::AssemblyOffset(emission.tier_entry),
    );
    let node_count = built.graph.nodes.len() as u64;
    let artifact = artifact_request.map(|request| {
        let mut code_map = crate::artifact::CodeMapCapture::default();
        let offsets = &emission.node_offsets;
        for (index, &(start, node)) in offsets.iter().enumerate() {
            let end = offsets
                .get(index + 1)
                .map_or(emission.body_end, |&(next, _)| next);
            let data = built.graph.node(node);
            let byte_pc = view
                .instructions
                .get(data.pc as usize)
                .map_or(0, |metadata| metadata.byte_pc);
            code_map.record(crate::artifact::CodeRegion::instruction(
                start,
                end,
                data.block.map(|block| block.0),
                None,
                view.code_block.id,
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
        )
    });
    Ok(crate::artifact::NativeCompileOutput {
        code: OptimizedCode::new(
            code,
            emission.call_entry,
            deopt,
            safepoints.into_boxed_slice(),
            osr_pc.into_iter().collect(),
            Box::new([]),
            emission.load_ic_cells,
            emission.store_ic_cells,
            plan.register_operands,
            metadata,
        ),
        artifact,
        diagnostics: Box::default(),
        ir_node_count: node_count,
    })
}

#[cfg(test)]
mod tests;
