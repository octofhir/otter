//! x86 native literal allocation with a canonical collecting miss.
//!
//! # Contents
//! - Prepared object/array fits over four allocator-owned GP temporaries.
//! - Direct committed span misses without a Generic register-window detour.
//!
//! # Invariants
//! - The candidate never aliases the result; its previous occupant reaches
//!   canonical homes before a collecting miss assigns the result.
//! - Plans and realms come from the node's actual outer or inlined source;
//!   each field's observation record comes from the node itself.
//! - The allocation site's compiled placement selects buffer and header; a
//!   tracking site leaves before effects once decided and counts each birth.
//! - Fit initialization precedes publication and cannot collect.
//! - Misses use the same canonical VM allocator and root contract as ARM.
//!
//! # See also
//! - [`crate::x86_64::allocation`] owns LAB encoding for both x86 tiers.
//! - [`super::calls`] owns the committed runtime/span boundary.

use super::*;
use crate::allocation::{AllocationValue, EmptyLiteralLayout, LabRegisters};
use crate::x86_64::allocation::{
    emit_array_literal, emit_boilerplate_array, emit_empty_literal, emit_object_literal,
};
use otter_vm::jit::{
    JIT_SITE_CREATED_OFFSET, JIT_SITE_DECIDED_BIT, JIT_SITE_DECISION_OFFSET, JitSiteMode,
};

impl Codegen<'_> {
    /// A regex literal's young birth from its baked image; the committed
    /// miss makes the RegExp in the runtime.
    pub(super) fn emit_regexp_allocation(&mut self, node: NodeId) -> Result<(), Unsupported> {
        let destination = Self::gp(self.loc(node).result.expect("regexp allocation result"));
        let Kind::NewRegExp(byte_pc) = self.graph.node(node).kind else {
            unreachable!("regexp allocation kind");
        };
        let source = self.view_of(node);
        let realm = source.literal_allocations.realm_id;
        let plan = *source
            .literal_allocations
            .regexps
            .get(&byte_pc)
            .ok_or(Unsupported::OperandShape("graph regexp literal plan"))?;
        let allocation = self.loc(node);
        let regs = LabRegisters {
            buffer: allocation.gp_temps[0],
            candidate: allocation.gp_temps[1],
            end: allocation.gp_temps[2],
            scratch: allocation.gp_temps[3],
            size: 11,
        };
        debug_assert!(!allocation.gp_temps.contains(&destination));
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        crate::x86_64::allocation::emit_regexp_literal(&mut self.ops, 15, realm, &plan, regs, slow);
        dynasm!(self.ops ; .arch x64 ; mov Rq(destination), Rq(regs.candidate) ; jmp =>done ; =>slow);
        self.emit_committed_call(
            node,
            abi::STUB_JIT_REGEXP_LITERAL,
            &[
                CommittedArgument::Scalar(u64::from(plan.constant)),
                CommittedArgument::Scalar(u64::from(source.code_block.id)),
            ],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }

    pub(super) fn emit_empty_allocation(&mut self, node: NodeId) -> Result<(), Unsupported> {
        let destination = Self::gp(self.loc(node).result.expect("empty allocation result"));
        let plans = &self.view_of(node).literal_allocations;
        let realm = plans.realm_id;
        let allocation = self.loc(node);
        let regs = LabRegisters {
            buffer: allocation.gp_temps[0],
            candidate: allocation.gp_temps[1],
            end: allocation.gp_temps[2],
            scratch: allocation.gp_temps[3],
            size: 11,
        };
        debug_assert!(!allocation.gp_temps.contains(&destination));
        let (layout, stub) = match self.graph.node(node).kind {
            Kind::NewObject => (
                plans.object.map(EmptyLiteralLayout::Object),
                abi::STUB_JIT_NEW_OBJECT,
            ),
            Kind::NewNullPrototypeObject => (
                plans.null_object.map(EmptyLiteralLayout::Object),
                abi::STUB_JIT_NEW_NULL_PROTOTYPE_OBJECT,
            ),
            Kind::NewArrayEmpty => (
                Some(EmptyLiteralLayout::Array(plans.array)),
                abi::STUB_JIT_NEW_ARRAY,
            ),
            _ => unreachable!("empty allocation kind"),
        };
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        if let Some(layout) = layout {
            emit_empty_literal(&mut self.ops, 15, realm, layout, regs, slow);
            dynasm!(self.ops ; .arch x64 ; mov Rq(destination), Rq(regs.candidate) ; jmp =>done);
        }
        dynasm!(self.ops ; .arch x64 ; =>slow);
        self.emit_committed_call(
            node,
            stub,
            &[CommittedArgument::Scalar(0), CommittedArgument::Scalar(0)],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }

    pub(super) fn emit_literal_allocation(&mut self, node: NodeId) -> Result<(), Unsupported> {
        let destination = Self::gp(self.loc(node).result.expect("literal allocation result"));
        let data = self.graph.node(node);
        let source = self.view_of(node);
        let realm = source.literal_allocations.realm_id;
        let cage_base = source.cage_base as u64;
        let object = source.literal_allocations.objects.get(&data.pc).cloned();
        // A spliced body's rest list has no instruction of its own to plan;
        // its geometry is its count's.
        let array = source
            .literal_allocations
            .arrays
            .get(&data.pc)
            .copied()
            .or_else(|| {
                (data.kind == Kind::NewArrayLiteral
                    && !source
                        .literal_allocations
                        .boilerplates
                        .contains_key(&data.pc))
                .then(|| otter_vm::jit::JitArrayLiteralAllocationPlan::new(data.inputs.len()))
                .flatten()
            });
        let boilerplate = source
            .literal_allocations
            .boilerplates
            .get(&data.pc)
            .copied();
        let site = source
            .literal_allocations
            .sites
            .get(&data.pc)
            .copied()
            .unwrap_or_default();
        let kind = data.kind.clone();
        let allocation = self.loc(node);
        let values = allocation.inputs.clone();
        let inputs = values
            .iter()
            .map(|&location| match location {
                Location::TaggedSlot(_) => Ok(AllocationValue::StackByte(
                    self.slots.offset(location) + self.sp_delta,
                )),
                Location::Constant(value) => match self.graph.node(value).kind {
                    Kind::ConstTagged(bits) => Ok(AllocationValue::Constant(bits)),
                    _ => Err(Unsupported::OperandShape("literal input is not tagged")),
                },
                _ => Err(Unsupported::OperandShape(
                    "literal input needs a canonical tagged home",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let regs = LabRegisters {
            buffer: allocation.gp_temps[0],
            candidate: allocation.gp_temps[1],
            end: allocation.gp_temps[2],
            scratch: allocation.gp_temps[3],
            size: 11,
        };
        debug_assert!(!allocation.gp_temps.contains(&destination));
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let fitted = self.ops.new_dynamic_label();
        let fits = match &kind {
            Kind::NewObjectLiteral { fields } => object.is_some() && fields.is_some(),
            Kind::NewArrayLiteral => array.is_some() || boilerplate.is_some(),
            _ => unreachable!("literal allocation kind"),
        };
        if fits && site.mode == JitSiteMode::Track {
            // A decided site leaves before any effect and recompiles.
            let exit = self.eager_exit(node, DeoptReason::AllocationSiteDecided);
            self.load_immediate(10, site.record);
            dynasm!(self.ops ; .arch x64
                ; test BYTE [r10 + JIT_SITE_DECISION_OFFSET as i32], JIT_SITE_DECIDED_BIT as i8
                ; jnz =>exit);
        }
        let stub = match kind {
            Kind::NewObjectLiteral { fields } => {
                if let Some((plan, fields)) = object.zip(fields) {
                    emit_object_literal(
                        &mut self.ops,
                        &mut self.relocations,
                        self.routines,
                        source,
                        15,
                        realm,
                        &plan,
                        site,
                        &fields,
                        &inputs,
                        regs,
                        slow,
                    );
                    dynasm!(self.ops ; .arch x64 ; jmp =>fitted);
                }
                abi::STUB_JIT_NEW_OBJECT_LITERAL
            }
            Kind::NewArrayLiteral => {
                if let Some(plan) = boilerplate {
                    emit_boilerplate_array(
                        &mut self.ops,
                        &mut self.relocations,
                        15,
                        realm,
                        cage_base,
                        plan,
                        site,
                        regs,
                        slow,
                    );
                    dynasm!(self.ops ; .arch x64 ; jmp =>fitted);
                } else if let Some(plan) = array {
                    emit_array_literal(
                        &mut self.ops,
                        &mut self.relocations,
                        15,
                        realm,
                        cage_base,
                        plan,
                        site,
                        &inputs,
                        regs,
                        slow,
                    );
                    dynasm!(self.ops ; .arch x64 ; jmp =>fitted);
                }
                abi::STUB_JIT_NEW_ARRAY
            }
            _ => unreachable!("literal allocation kind"),
        };
        if fits {
            dynasm!(self.ops ; .arch x64 ; =>fitted);
            if site.mode == JitSiteMode::Track {
                // The tagged birth counts toward the site's survival ratio.
                self.load_immediate(10, site.record);
                dynasm!(self.ops ; .arch x64
                    ; add DWORD [r10 + JIT_SITE_CREATED_OFFSET as i32], 1);
            }
            dynasm!(self.ops ; .arch x64 ; mov Rq(destination), Rq(regs.candidate) ; jmp =>done);
        }
        dynasm!(self.ops ; .arch x64 ; =>slow);
        self.emit_committed_span_call(node, stub, &values, destination)?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }
}

mod lexical;

mod primitive;

mod group;

mod receiver;
