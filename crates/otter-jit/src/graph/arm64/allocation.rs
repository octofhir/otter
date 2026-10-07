//! Native literal allocation with a canonical collecting miss.
//!
//! # Contents
//! - Object/array fit using VM-prepared geometry and declared temporaries.
//! - Direct committed span ABI with no interpreter register-window publication.
//!
//! # Invariants
//! The candidate never aliases the result; the result's previous occupant stays
//! intact until cold canonical saves. The own source realm guards the fit, and
//! the existing VM allocator selects that same source realm on a miss.
//!
//! # See also
//! `crate::arm64::allocation` owns the one LAB probe/init/publish implementation.

use super::{Codegen, CommittedArgument, Kind, Location, NodeId, abi};
use crate::allocation::{AllocationValue, EmptyLiteralLayout, LabRegisters};
use crate::arm64::allocation::{emit_array_literal, emit_empty_literal, emit_object_literal};
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};

impl Codegen<'_> {
    pub(super) fn emit_literal_allocation(
        &mut self,
        node: NodeId,
        destination: u8,
    ) -> Result<(), crate::Unsupported> {
        let data = self.graph.node(node);
        let source = self.view_of(node);
        let realm = source.literal_allocations.realm_id;
        let cage_base = source.cage_base as u64;
        let object = source.literal_allocations.objects.get(&data.pc).copied();
        let array = source.literal_allocations.arrays.get(&data.pc).copied();
        let kind = data.kind.clone();
        let allocation = self.allocation.node(node);
        let values = allocation.inputs.clone();
        let inputs = values
            .iter()
            .map(|location| match *location {
                Location::TaggedSlot(_) => Ok(AllocationValue::StackByte(
                    self.slots.offset(*location) + self.sp_delta,
                )),
                Location::Constant(value) => match self.graph.node(value).kind {
                    Kind::ConstTagged(bits) => Ok(AllocationValue::Constant(bits)),
                    _ => Err(crate::Unsupported::OperandShape(
                        "literal input is not tagged",
                    )),
                },
                _ => Err(crate::Unsupported::OperandShape(
                    "literal input needs a canonical tagged home",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let regs = LabRegisters {
            buffer: allocation.gp_temps[0],
            candidate: allocation.gp_temps[1],
            end: allocation.gp_temps[2],
            scratch: allocation.gp_temps[3],
            size: 17,
        };
        debug_assert!(!allocation.gp_temps.contains(&destination));
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let stub = match kind {
            Kind::NewObjectLiteral => {
                if let Some(plan) = object {
                    emit_object_literal(&mut self.ops, 20, realm, plan, &inputs, regs, slow);
                    dynasm!(self.ops ; .arch aarch64 ; mov X(destination), X(regs.candidate) ; b =>done);
                }
                abi::STUB_JIT_NEW_OBJECT_LITERAL
            }
            Kind::NewArrayLiteral => {
                if let Some(plan) = array {
                    emit_array_literal(
                        &mut self.ops,
                        &mut self.relocations,
                        20,
                        realm,
                        cage_base,
                        plan,
                        &inputs,
                        regs,
                        slow,
                    );
                    dynasm!(self.ops ; .arch aarch64 ; mov X(destination), X(regs.candidate) ; b =>done);
                }
                abi::STUB_JIT_NEW_ARRAY
            }
            _ => unreachable!("literal allocation kind"),
        };
        dynasm!(self.ops ; .arch aarch64 ; =>slow);
        self.emit_committed_span_call(node, stub, &values, destination)?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    pub(super) fn emit_empty_allocation(
        &mut self,
        node: NodeId,
        destination: u8,
    ) -> Result<(), crate::Unsupported> {
        let plans = &self.view_of(node).literal_allocations;
        let realm_id = plans.realm_id;
        let allocation = self.allocation.node(node);
        let regs = LabRegisters {
            buffer: allocation.gp_temps[0],
            candidate: allocation.gp_temps[1],
            end: allocation.gp_temps[2],
            scratch: allocation.gp_temps[3],
            size: 17,
        };
        debug_assert!(!allocation.gp_temps.contains(&destination));
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let (layout, stub) = match self.graph.node(node).kind {
            Kind::NewObject => (
                plans.object.map(EmptyLiteralLayout::Object),
                abi::STUB_JIT_NEW_OBJECT,
            ),
            Kind::NewArrayEmpty => (
                Some(EmptyLiteralLayout::Array(plans.array)),
                abi::STUB_JIT_NEW_ARRAY,
            ),
            _ => unreachable!("empty allocation kind"),
        };
        if let Some(layout) = layout {
            emit_empty_literal(&mut self.ops, 20, realm_id, layout, regs, slow);
            dynasm!(self.ops ; .arch aarch64 ; mov X(destination), X(regs.candidate) ; b =>done);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>slow);
        self.emit_committed_call(
            node,
            stub,
            &[CommittedArgument::Scalar(0), CommittedArgument::Scalar(0)],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
    }
}

mod lexical;

mod primitive;

mod group;

mod receiver;
