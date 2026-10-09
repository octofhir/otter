//! Native x86-64 Graph instruction encoding over the shared SSA contracts.
//!
//! # Contents
//! - Canonical homes, parallel moves and ordinary/OSR entry publication.
//! - Target instruction modules for control, scalars, memory and calls.
//! - Finalized bytes and metadata through the one Graph emission owner.
//!
//! # Invariants
//! - R15 is context, R14 frame, R13 window; R10/R11 and XMM15 are scratch.
//! - Collecting transitions preserve values only in traced canonical homes.
//! - Stack staging is aligned and every slot access includes its current delta.
//! - Private JS entries and platform C calls retain their distinct shared ABIs.
//!
//! # See also
//! - `super::registers::X86_64` owns physical allocation constraints.
//! - `crate::x86_64::frame` owns native frame creation and retirement.

// Dynamic register operands are already u8; dynasm preserves an Into conversion.
#![allow(clippy::useless_conversion)]

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, native_abi as abi, value::tag};
use rustc_hash::FxHashMap;

use super::builder::Built;
use super::call::CommittedArgument;
use super::emission::{Emission, ExitSite};
use super::frame::{SlotLayout, exit_reason};
use super::ir::{BlockId, BranchKind, Condition, DeoptReason, Graph, Kind, NodeId, Repr};
use super::metadata::{self, FIRST_SITE_SAFEPOINT};
use super::regalloc::{Allocation, Location, Move};
use crate::Unsupported;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::x86_64::values::{emit_load_symbol_u64, emit_load_u64};

mod allocation;
mod barriers;
mod binding;
mod calls;
mod control;
mod elements;
mod exits;
mod guards;
mod homes;
mod keyed_load;
mod keyed_store;
mod memory;
mod native_leaf;
mod properties;
mod scalar;

type Deferred<'a> = Box<dyn FnOnce(&mut Codegen<'a>) + 'a>;

struct Codegen<'a> {
    ops: Assembler,
    relocations: RelocationCapture,
    view: &'a JitCompileSnapshot,
    inline_views: &'a [std::sync::Arc<JitCompileSnapshot>],
    graph: &'a Graph,
    allocation: &'a Allocation,
    layout: &'a [BlockId],
    slots: SlotLayout,
    sp_delta: u32,
    labels: FxHashMap<BlockId, DynamicLabel>,
    exits: Vec<ExitSite>,
    deferred: Vec<Deferred<'a>>,
    returned: DynamicLabel,
    deopt: DynamicLabel,
    fatal: DynamicLabel,
    activation: crate::frame::ActivationExits,
    spill: crate::frame::SpillArea,
    transitions: &'a crate::entry::TransitionTable,
    deopt_runtime: u64,
    plan: &'a crate::template::TemplatePlan,
    plan_index: FxHashMap<u32, Vec<usize>>,
    /// Shared property routines requested by Generic sites.
    shared_property: crate::template::x86_64::shared_property::SharedPropertyProbes,
    no_direct_call_events: Option<crate::template::DirectCallEvents>,
    no_code_map: Option<crate::artifact::CodeMapCapture>,
    spliced_functions: std::collections::BTreeSet<u32>,
    node_offsets: Vec<(usize, NodeId)>,
    threw: DynamicLabel,
    committed_throw: DynamicLabel,
    propagate: DynamicLabel,
    materialize: DynamicLabel,
    /// Exact root records of the collecting boundaries emitted so far.
    sites: metadata::SitePlan,
    return_sites: Vec<abi::SafepointEntry>,
}

/// Encode the production graph, with one entry and one recovery contract.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit(
    view: &JitCompileSnapshot,
    built: &Built,
    allocation: &Allocation,
    transitions: &crate::entry::TransitionTable,
    code_object_id: u64,
    deopt_runtime: *const otter_vm::deopt::DeoptRuntime,
    plan: &crate::template::TemplatePlan,
    slots: SlotLayout,
    capture_relocations: bool,
) -> Result<Emission, Unsupported> {
    metadata::validate_generic_sources(&built.graph)?;
    slots.validate(view.code_block.register_count)?;
    let mut ops = Assembler::new()
        .map_err(|_| Unsupported::Backend(crate::BackendFailure::AssemblerAllocation))?;
    let labels = built
        .layout
        .iter()
        .map(|&id| (id, ops.new_dynamic_label()))
        .collect();
    let returned = ops.new_dynamic_label();
    let deopt = ops.new_dynamic_label();
    let fatal = ops.new_dynamic_label();
    let activation = crate::frame::ActivationExits {
        construct: ops.new_dynamic_label(),
        side_exit: ops.new_dynamic_label(),
    };
    let threw = ops.new_dynamic_label();
    let committed_throw = ops.new_dynamic_label();
    let propagate = ops.new_dynamic_label();
    let materialize = ops.new_dynamic_label();
    // Both entries zero the exception scratch, the only tagged word the
    // entry record roots; every other home is rooted only once written.
    let spill = crate::frame::SpillArea {
        bytes: slots.bytes(),
        scratch_slot: Some(slots.spill_tagged),
        safepoint: FIRST_SITE_SAFEPOINT,
        saved_pairs: 0,
    };
    let lazy_window = !built
        .graph
        .nodes
        .iter()
        .any(|node| matches!(node.kind, Kind::Generic { .. } | Kind::LoadWindow(_)));
    let shape = crate::call_linkage::EntryShape::of(
        view,
        code_object_id,
        abi::NativeFrameKind::Optimizing,
        true,
    )?
    .with_lazy_window(lazy_window);
    let mut codegen = Codegen {
        ops,
        relocations: RelocationCapture::new(capture_relocations),
        view,
        inline_views: &built.inline_views,
        graph: &built.graph,
        allocation,
        layout: &built.layout,
        slots,
        sp_delta: 0,
        labels,
        exits: Vec::new(),
        deferred: Vec::new(),
        returned,
        deopt,
        fatal,
        activation,
        spill,
        transitions,
        deopt_runtime: deopt_runtime as u64,
        plan,
        plan_index: metadata::operation_index(plan),
        shared_property: Default::default(),
        no_direct_call_events: None,
        no_code_map: None,
        spliced_functions: std::collections::BTreeSet::new(),
        node_offsets: Vec::new(),
        threw,
        committed_throw,
        propagate,
        materialize,
        sites: metadata::SitePlan::new(slots.spill_tagged),
        return_sites: Vec::new(),
    };
    let tier_entry = codegen.ops.offset().0;
    let body = codegen.ops.new_dynamic_label();
    crate::x86_64::frame::emit_tier_prologue(&mut codegen.ops, shape.kind, spill);
    let osr_dispatch_end = if let Some(osr) = built.osr_entry {
        let osr_label = codegen.labels[&osr];
        let dispatch = codegen.ops.new_dynamic_label();
        dynasm!(codegen.ops ; .arch x64
            ; test BYTE [r14 + crate::entry::NATIVE_FRAME_FLAGS_OFFSET as i32], abi::NativeFrameFlags::OSR_ENTRY as i8
            ; jnz =>dispatch ; jmp =>body ; =>dispatch
            ; and BYTE [r14 + crate::entry::NATIVE_FRAME_FLAGS_OFFSET as i32], !(abi::NativeFrameFlags::OSR_ENTRY as i8)
            ; jmp =>osr_label
        );
        Some(codegen.ops.offset().0)
    } else {
        dynasm!(codegen.ops ; .arch x64 ; jmp =>body);
        None
    };
    let cold = crate::frame::CallEntryCold::new(&mut codegen.ops, shape);
    crate::x86_64::frame::emit_call_entry_cold(
        &mut codegen.ops,
        &mut codegen.relocations,
        transitions,
        view,
        shape,
        activation,
        cold,
    );
    let call_entry = crate::x86_64::frame::emit_call_entry(
        &mut codegen.ops,
        &mut codegen.relocations,
        view,
        shape,
        spill,
        cold,
    )
    .0;
    dynasm!(codegen.ops ; .arch x64 ; =>body);
    codegen.emit_body()?;
    let body_end = codegen.ops.offset().0;
    codegen.emit_return_path();
    crate::x86_64::frame::emit_exits(
        &mut codegen.ops,
        &mut codegen.relocations,
        transitions,
        view,
        shape,
        activation,
        spill,
    );
    codegen.emit_exit_stubs();
    let shared_property = std::mem::take(&mut codegen.shared_property);
    shared_property.emit(
        &mut codegen.ops,
        &mut codegen.relocations,
        transitions,
        view,
    );
    codegen
        .return_sites
        .sort_by_key(|site| site.native_return_offset);
    let Codegen {
        ops,
        relocations,
        exits,
        node_offsets,
        sites,
        return_sites,
        spliced_functions,
        ..
    } = codegen;
    Ok(Emission {
        buffer: crate::entry::finalize_assembler(ops)?,
        tier_entry,
        call_entry,
        exits,
        relocations,
        node_offsets,
        body_end,
        osr_dispatch_end,
        site_records: sites.into_records(),
        return_sites,
        spliced_functions,
    })
}

impl<'a> Codegen<'a> {
    fn view_of(&self, node: NodeId) -> &'a JitCompileSnapshot {
        metadata::source_view(self.view, self.inline_views, self.graph, node)
    }
    fn gp(location: Location) -> u8 {
        match location {
            Location::Gp(r) => r,
            other => unreachable!("expected GP: {other:?}"),
        }
    }
    fn fp(location: Location) -> u8 {
        match location {
            Location::Fp(r) => r,
            other => unreachable!("expected FP: {other:?}"),
        }
    }
    fn load_immediate(&mut self, register: u8, bits: u64) {
        emit_load_u64(&mut self.ops, register, bits);
    }
    /// Retain this JS site's exact roots and source.
    fn node_safepoint(&mut self, node: NodeId) -> Result<abi::SafepointId, Unsupported> {
        self.sites.source(
            self.view,
            self.inline_views,
            self.graph,
            self.allocation,
            node,
        )
    }
    /// The exact record of an active C helper at `node`.
    fn helper_safepoint(&mut self, node: NodeId) -> Result<abi::SafepointId, Unsupported> {
        self.sites.helper(
            self.view,
            self.inline_views,
            self.graph,
            self.allocation,
            node,
        )
    }
    fn emit_stamp(&mut self, id: abi::SafepointId) {
        dynasm!(self.ops ; .arch x64 ; mov DWORD [r14+abi::NATIVE_FRAME_CALL_SITE_OFFSET as i32],id as i32);
    }
    fn stamp_node_safepoint(&mut self, node: NodeId) -> Result<abi::SafepointId, Unsupported> {
        let id = self.helper_safepoint(node)?;
        self.emit_stamp(id);
        Ok(id)
    }
    fn record_js_return(
        &mut self,
        node: NodeId,
        offset: dynasmrt::AssemblyOffset,
    ) -> Result<(), Unsupported> {
        let safepoint_id = self.node_safepoint(node)?;
        crate::return_sites::ReturnSiteRecorder {
            entries: &mut self.return_sites,
            safepoint_id,
            logical_pc: self.graph.outer_pc(node),
        }
        .record(offset)
    }

    fn stamp_pc(&mut self, node: NodeId) {
        let pc = self.graph.outer_pc(node);
        dynasm!(self.ops ; .arch x64
            ; mov DWORD [r14 + crate::entry::NATIVE_FRAME_PC_OFFSET as i32], pc as i32
        );
    }
    fn loc(&self, node: NodeId) -> &super::regalloc::NodeAllocation {
        self.allocation.node(node)
    }

    fn emit_body(&mut self) -> Result<(), Unsupported> {
        let entry = self.layout.first().copied();
        let mut forwarders: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
        let mut order = Vec::new();
        for &block in self.layout {
            let target = super::emission::forwarded(self.graph, self.allocation, block);
            if target != block && Some(block) != entry && !self.graph.block(block).is_loop {
                forwarders.entry(target).or_default().push(block);
            } else {
                order.push(block);
            }
        }
        for (position, &block) in order.iter().enumerate() {
            let label = self.labels[&block];
            dynasm!(self.ops ; .arch x64 ; =>label);
            for alias in forwarders.remove(&block).unwrap_or_default() {
                let label = self.labels[&alias];
                dynasm!(self.ops ; .arch x64 ; =>label);
            }
            let data = self.graph.block(block);
            for &phi in &data.phis {
                if !self.allocation.node(phi).skipped {
                    self.spill_definition(phi);
                }
            }
            for &node in &data.body {
                if self.allocation.node(node).skipped {
                    continue;
                }
                self.node_offsets.push((self.ops.offset().0, node));
                for mov in self.allocation.node(node).moves.clone() {
                    self.emit_move(mov.from, mov.to);
                }
                self.emit_node(node)?;
                self.spill_definition(node);
            }
            let control = data.control.expect("terminated block");
            self.node_offsets.push((self.ops.offset().0, control));
            for mov in self.allocation.node(control).moves.clone() {
                self.emit_move(mov.from, mov.to);
            }
            self.emit_control(block, control, order.get(position + 1).copied())?;
        }
        debug_assert!(forwarders.is_empty());
        Ok(())
    }
    fn spill_definition(&mut self, node: NodeId) {
        if let (Some(result), Some(&home)) = (
            self.allocation.node(node).result,
            self.allocation.spill.get(&node),
        ) && result != home
            && self.allocation.definition_spills.contains(&node)
        {
            self.emit_move(result, home);
        }
    }
    fn emit_node(&mut self, node: NodeId) -> Result<(), Unsupported> {
        if self.emit_guard(node)? || self.emit_memory(node)? || self.emit_scalar(node)? {
            return Ok(());
        }
        match &self.graph.node(node).kind {
            Kind::InitialRegister(register) | Kind::LoadWindow(register) => {
                let dst = Self::gp(self.loc(node).result.expect("result"));
                dynasm!(self.ops ; .arch x64 ; mov Rq(dst), [r13 + i32::from(*register) * 8]);
            }
            Kind::LoadThis | Kind::LoadClosure | Kind::LoadNewTarget => {
                let dst = Self::gp(self.loc(node).result.expect("result"));
                let offset = if self.graph.node(node).kind == Kind::LoadThis {
                    crate::entry::NATIVE_FRAME_THIS_OFFSET
                } else if self.graph.node(node).kind == Kind::LoadNewTarget {
                    crate::entry::NATIVE_FRAME_NEW_TARGET_OFFSET
                } else {
                    crate::entry::NATIVE_FRAME_SELF_OFFSET
                };
                dynasm!(self.ops ; .arch x64 ; mov Rq(dst), [r14 + offset as i32]);
            }
            Kind::LoadGlobalBinding(byte_pc) => self.emit_global_binding(node, *byte_pc)?,
            Kind::LoadGlobalThis => self.emit_global_this(node),
            Kind::LoadLiteral(byte_pc) => {
                let byte_pc = *byte_pc;
                let source = self.view_of(node);
                let cell = source
                    .literal_cells
                    .get(&byte_pc)
                    .ok_or(Unsupported::OperandShape("graph string cell"))?
                    .cell_addr;
                let dst = Self::gp(self.loc(node).result.expect("result"));
                emit_load_symbol_u64(
                    &mut self.ops,
                    &mut self.relocations,
                    dst,
                    cell as u64,
                    RelocationTarget::LiteralCell {
                        function_id: source.code_block.id,
                        byte_pc,
                    },
                );
                dynasm!(self.ops ; .arch x64 ; mov Rq(dst), [Rq(dst)]);
            }
            Kind::AllocationGroup(index) => self.emit_allocation_group(node, *index)?,
            Kind::AllocationProjection(byte) => self.emit_allocation_projection(node, *byte),
            Kind::NativeLeaf(stub) => self.emit_native_leaf(node, *stub)?,
            Kind::PrimitiveAdd => self.emit_primitive_add(node)?,
            Kind::BigIntBinary(operator) => self.emit_bigint_binary(node, *operator)?,
            Kind::PrimitiveCompare(condition) => self.emit_primitive_compare(node, *condition)?,
            Kind::NewObject | Kind::NewArrayEmpty => self.emit_empty_allocation(node)?,
            Kind::NewObjectLiteral | Kind::NewArrayLiteral => self.emit_literal_allocation(node)?,
            Kind::NewReceiver(plan) => self.emit_new_receiver(node, *plan)?,
            Kind::NativeNewContext(_)
            | Kind::CopyContext
            | Kind::NewClosure
            | Kind::NewArrayWithLength => {
                let dst = Self::gp(self.loc(node).result.expect("lexical allocation result"));
                self.emit_lexical_allocation(node, dst)?;
            }
            Kind::CallJs {
                pc,
                plan,
                construct,
                receiver,
                allocation,
            } => {
                self.emit_call_js(node, *pc, *plan, *construct, *receiver, *allocation)?;
            }
            Kind::CallForward { pc, plan, bindings } => {
                self.emit_call_forward(node, *pc, *plan, bindings)?
            }
            Kind::Generic { pc, registers } => self.emit_generic(node, *pc, registers)?,
            Kind::ConstTagged(_) | Kind::ConstInt32(_) | Kind::ConstFloat64(_) | Kind::Phi => {}
            _ => return Err(Unsupported::OperandShape("graph x86 node")),
        }
        Ok(())
    }
}
