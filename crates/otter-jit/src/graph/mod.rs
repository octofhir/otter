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
use otter_vm::deopt::{
    DeoptExitDescriptor, DeoptFrame, DeoptRuntime, DeoptTable, FrameState,
};
use otter_vm::native_abi::{
    NO_CALL_PC, NO_FRAME_STATE, SafepointRecord, TaggedLocation, TaggedLocationKind,
};

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

/// Compile `view` to machine code.
pub(crate) fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
    osr_pc: Option<u32>,
) -> Result<Compiled, Unsupported> {
    let analysis = bytecode::Analysis::build(view)
        .map_err(|_| Unsupported::OperandShape("graph bytecode analysis"))?;
    let plan = crate::template::TemplatePlan::build_unfused(view)?;
    let baseline = BaselineSupport::of(&plan, view);
    let mut built = builder::build(view, &analysis, &baseline, osr_pc)
        .map_err(|_| Unsupported::OperandShape("graph construction"))?;
    phi_repr::untag_phis(&mut built.graph, &built.layout);
    let allocation = regalloc::allocate(&built.graph, &built.layout);
    // The recipe's address is baked before its contents exist: the exits are
    // known only once the code is emitted, and the box never moves.
    let mut deopt = Box::new(DeoptRuntime::default());
    let slots = arm64::SlotLayout::of(&allocation);
    let emission = arm64::emit(
        view,
        &built,
        &allocation,
        transitions,
        code_object_id,
        std::ptr::from_ref::<DeoptRuntime>(&deopt),
        &plan,
        slots,
        false,
    )?;
    *deopt = deopt_runtime(view, &built, &allocation, slots, &emission.exits);
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
    view: &JitCompileSnapshot,
    built: &builder::Built,
    allocation: &regalloc::Allocation,
    slots: arm64::SlotLayout,
    exits: &[arm64::ExitSite],
) -> DeoptRuntime {
    let mut states = Vec::with_capacity(exits.len());
    let mut descriptors = Vec::with_capacity(exits.len());
    for (index, site) in exits.iter().enumerate() {
        let node = built.graph.node(site.node);
        let state_id = if site.lazy { node.lazy } else { node.eager }
            .expect("an exit site has its frame state");
        let state = built.graph.frame_state(state_id);
        let node_allocation = allocation.node(site.node);
        let locations = if site.lazy {
            &node_allocation.lazy
        } else {
            &node_allocation.eager
        };
        states.push(FrameState {
            frames: Box::new([DeoptFrame {
                function_id: view.code_block.id,
                byte_pc: state.byte_pc,
                entry: None,
                slots: arm64::deopt_slots(&built.graph, slots, state, locations),
            }]),
            virtual_objects: Box::new([]),
        });
        descriptors.push(DeoptExitDescriptor {
            state: index as u32,
            reason: site.reason,
            action: site.action,
            resume_pcs: Box::new([state.pc]),
        });
    }
    DeoptRuntime {
        table: DeoptTable::from_states(states),
        exits: descriptors.into_boxed_slice(),
        gpr_budget: regalloc::GP_REGISTERS.len() as u16,
    }
}

/// Compile `view` into an installable optimized code object.
pub(crate) fn compile_optimized(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &crate::entry::TransitionTable,
    osr_pc: Option<u32>,
) -> Result<crate::artifact::NativeCompileOutput<OptimizedCode>, Unsupported> {
    let compiled = compile(view, code_object_id, transitions, osr_pc)?;
    let Compiled {
        emission,
        built,
        allocation,
        deopt,
        slots,
        plan,
    } = compiled;
    let mut safepoints = plan.safepoint_records.clone();
    safepoints.push(SafepointRecord {
        id: arm64::SLOT_SAFEPOINT,
        frame_state: NO_FRAME_STATE,
        tagged_locations: (0..slots.tagged)
            .map(|index| TaggedLocation {
                kind: TaggedLocationKind::SpillSlot,
                index: index as u16,
            })
            .collect(),
        inline_frames: Box::new([]),
        call_pc: NO_CALL_PC,
    });
    safepoints.sort_by_key(|record| record.id);
    let metadata = OptimizedMetadata {
        code_object_id,
        function_id: view.code_block.id,
        param_count: view.code_block.param_count,
        register_count: view.code_block.register_count,
        machine_register_count: (regalloc::GP_REGISTERS.len() + regalloc::FP_REGISTERS.len())
            as u8,
        allocator_spill_slot_count: allocation.tagged_slots + allocation.untagged_slots,
        spill_slot_count: allocation.tagged_slots + allocation.untagged_slots,
    };
    let code = CompiledCode::new(
        emission.buffer,
        dynasmrt::AssemblyOffset(emission.tier_entry),
    );
    let node_count = built.graph.nodes.len() as u64;
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
        artifact: None,
        diagnostics: Box::default(),
        ir_node_count: node_count,
    })
}

#[cfg(test)]
mod tests;
