//! Backend-neutral planning for straight-line template method inlining.
//!
//! # Contents
//! - [`InlineMethodPlan`] validates the small replay-safe numeric method subset.
//! - [`InlineScratchLayout`] maps sparse callee virtual registers onto a compact
//!   deterministic stack-slot set.
//! - [`InlineEntryValue`] describes the exact argument, receiver, and
//!   `undefined` values that must be materialized at inline-body entry.
//!
//! # Invariants
//! - Accepted bodies are straight-line, read-only, and end in exactly one final
//!   return. An emitter may guard and replay them only before any observable
//!   effect.
//! - Liveness models every instruction as source reads followed by its
//!   destination write. Distinct virtual registers may therefore share a slot
//!   at that boundary only when the emitter loads every source before storing
//!   the destination.
//! - The layout is a static virtual-register map, not an SSA or versioned
//!   location stack. A later definition of one virtual register always uses the
//!   same compact slot, and interference accounts for values live across it.
//! - Scratch values are not published through the active VM frame. Backends
//!   consuming this plan must keep the inline body call-free, allocation-free,
//!   and safepoint-free.
//! - The receiver entry value is the boxed call receiver. Prototype holders
//!   used by method-identity guards never replace it as the `this` value.
//!
//! # See also
//! - [`super::TemplatePlan`] — validated typed operations analyzed here.
//! - `super::arm64` — current machine-code consumer of this plan.

use std::cmp::Reverse;

use otter_vm::JitInlineMethod;

use super::plan::TemplateInstr;
use super::{ArithKind, TemplateOp, TemplatePlan};

pub(crate) const INLINE_METHOD_MAX_REGISTERS: u16 = 24;
pub(crate) const INLINE_METHOD_MAX_INSTRUCTIONS: usize = 48;
pub(crate) const INLINE_METHOD_MAX_ARGUMENTS: usize = 2;

const INLINE_SCRATCH_SLOT_BYTES: u32 = 8;
const INLINE_STACK_ALIGNMENT: u32 = 16;

/// One compact stack slot used by an inline method body.
///
/// This type intentionally differs from the callee's virtual-register index:
/// passing an untranslated virtual register to a scratch load/store should be
/// a type error at the backend boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct InlineScratchSlot(u16);

impl InlineScratchSlot {
    #[must_use]
    pub(crate) const fn index(self) -> u16 {
        self.0
    }

    #[must_use]
    pub(crate) const fn byte_offset(self) -> u32 {
        self.0 as u32 * INLINE_SCRATCH_SLOT_BYTES
    }
}

/// One value copied into compact scratch storage before inline execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InlineEntryValue {
    /// Copy caller argument `argument` into the slot for callee `register`.
    Argument {
        argument: u16,
        register: u16,
        slot: InlineScratchSlot,
    },
    /// Copy the boxed method receiver used by `LoadThis`.
    Receiver { slot: InlineScratchSlot },
    /// Initialize a non-parameter register read before its first write.
    Undefined {
        register: u16,
        slot: InlineScratchSlot,
    },
}

/// Compact deterministic storage assignment for one accepted inline body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InlineScratchLayout {
    register_slots: Box<[Option<InlineScratchSlot>]>,
    receiver_slot: Option<InlineScratchSlot>,
    entry_values: Box<[InlineEntryValue]>,
    slot_count: u16,
}

impl InlineScratchLayout {
    /// Compact slot for one callee virtual register, or `None` when the body
    /// never reads or writes that register.
    #[must_use]
    pub(crate) fn register_slot(&self, register: u16) -> Option<InlineScratchSlot> {
        self.register_slots
            .get(usize::from(register))
            .copied()
            .flatten()
    }

    /// Slot retaining the boxed receiver for `LoadThis`, when used.
    #[must_use]
    pub(crate) const fn receiver_slot(&self) -> Option<InlineScratchSlot> {
        self.receiver_slot
    }

    /// Exact entry values required by liveness, in deterministic order:
    /// arguments, receiver, then `undefined` locals.
    #[must_use]
    pub(crate) fn entry_values(&self) -> &[InlineEntryValue] {
        &self.entry_values
    }

    #[must_use]
    pub(crate) const fn slot_count(&self) -> u16 {
        self.slot_count
    }

    /// AArch64-compatible aligned byte size for the compact scratch block.
    ///
    /// Zero slots produce zero bytes, allowing a return-undefined body to avoid
    /// changing `sp` at all.
    #[must_use]
    pub(crate) const fn aligned_bytes(&self) -> u32 {
        let bytes = self.slot_count as u32 * INLINE_SCRATCH_SLOT_BYTES;
        (bytes + INLINE_STACK_ALIGNMENT - 1) & !(INLINE_STACK_ALIGNMENT - 1)
    }
}

/// Validated straight-line method body plus its compact scratch layout.
pub(crate) struct InlineMethodPlan<'a> {
    template: &'a TemplatePlan,
    scratch: InlineScratchLayout,
    has_receiver_property: bool,
}

impl<'a> InlineMethodPlan<'a> {
    /// Validate one baked method and derive its compact static scratch layout.
    ///
    /// `None` means the body or call shape is outside the replay-safe inline
    /// subset. The supplied [`TemplatePlan`] remains authoritative for typed
    /// operands and instruction order.
    #[must_use]
    pub(crate) fn build(
        method: &JitInlineMethod,
        template: &'a TemplatePlan,
        argc: usize,
    ) -> Option<Self> {
        if argc != usize::from(method.param_count)
            || argc > INLINE_METHOD_MAX_ARGUMENTS
            || argc > usize::from(method.register_count)
            || method.register_count > INLINE_METHOD_MAX_REGISTERS
            || template.instructions.len() > INLINE_METHOD_MAX_INSTRUCTIONS
            || template.register_count != method.register_count
        {
            return None;
        }

        let register_count = usize::from(method.register_count);
        let receiver_node = register_count;
        let receiver_read = node_bit(receiver_node)?;
        let mut kinds = vec![InlineValueKind::Unknown; register_count];
        let mut accesses = Vec::with_capacity(template.instructions.len());
        let mut has_receiver_property = false;
        let mut saw_return = false;

        for (index, instruction) in template.instructions.iter().enumerate() {
            if saw_return {
                return None;
            }
            let access = match instruction.op {
                TemplateOp::LoadImmediate { dst, .. } => {
                    write_kind(&mut kinds, dst, InlineValueKind::Unknown)?;
                    InlineAccess::write(dst, method.register_count)?
                }
                TemplateOp::Move { dst, src } => {
                    let kind = read_kind(&kinds, src)?;
                    write_kind(&mut kinds, dst, kind)?;
                    InlineAccess::read_write(&[src], dst, method.register_count)?
                }
                TemplateOp::LoadThis { dst } => {
                    write_kind(&mut kinds, dst, InlineValueKind::Receiver)?;
                    InlineAccess {
                        reads: receiver_read,
                        dst: Some(dst),
                    }
                }
                TemplateOp::LoadProperty { dst, object, .. } => {
                    if read_kind(&kinds, object)? != InlineValueKind::Receiver
                        || !method.prop_offsets.contains_key(&instruction.byte_pc)
                        || method.prop_shapes.contains_key(&instruction.byte_pc)
                    {
                        return None;
                    }
                    write_kind(&mut kinds, dst, InlineValueKind::Unknown)?;
                    has_receiver_property = true;
                    InlineAccess::read_write(&[object], dst, method.register_count)?
                }
                TemplateOp::ToPrimitive { dst, src, .. } | TemplateOp::ToNumeric { dst, src } => {
                    read_kind(&kinds, src)?;
                    write_kind(&mut kinds, dst, InlineValueKind::Unknown)?;
                    InlineAccess::read_write(&[src], dst, method.register_count)?
                }
                TemplateOp::AddGeneric { dst, lhs, rhs, .. } => {
                    read_kind(&kinds, lhs)?;
                    read_kind(&kinds, rhs)?;
                    write_kind(&mut kinds, dst, InlineValueKind::Unknown)?;
                    InlineAccess::read_write(&[lhs, rhs], dst, method.register_count)?
                }
                TemplateOp::BinaryArith {
                    dst,
                    lhs,
                    rhs,
                    kind,
                } => {
                    if matches!(kind, ArithKind::Rem | ArithKind::Pow) {
                        return None;
                    }
                    read_kind(&kinds, lhs)?;
                    read_kind(&kinds, rhs)?;
                    write_kind(&mut kinds, dst, InlineValueKind::Unknown)?;
                    InlineAccess::read_write(&[lhs, rhs], dst, method.register_count)?
                }
                TemplateOp::Return { src } => {
                    read_kind(&kinds, src)?;
                    saw_return = true;
                    if index + 1 != template.instructions.len() {
                        return None;
                    }
                    InlineAccess::read(&[src], method.register_count)?
                }
                TemplateOp::ReturnUndefined => {
                    saw_return = true;
                    if index + 1 != template.instructions.len() {
                        return None;
                    }
                    InlineAccess::none()
                }
                _ => return None,
            };
            accesses.push(access);
        }

        if !saw_return {
            return None;
        }
        let scratch = build_scratch_layout(method.register_count, argc, &accesses)?;
        Some(Self {
            template,
            scratch,
            has_receiver_property,
        })
    }

    #[must_use]
    pub(crate) fn instructions(&self) -> &[TemplateInstr] {
        &self.template.instructions
    }

    #[must_use]
    pub(crate) fn entry_values(&self) -> &[InlineEntryValue] {
        self.scratch.entry_values()
    }

    #[must_use]
    pub(crate) fn register_slot(&self, register: u16) -> Option<InlineScratchSlot> {
        self.scratch.register_slot(register)
    }

    #[must_use]
    pub(crate) const fn receiver_slot(&self) -> Option<InlineScratchSlot> {
        self.scratch.receiver_slot()
    }

    #[must_use]
    pub(crate) const fn slot_count(&self) -> u16 {
        self.scratch.slot_count()
    }

    #[must_use]
    pub(crate) const fn aligned_scratch_bytes(&self) -> u32 {
        self.scratch.aligned_bytes()
    }

    #[must_use]
    pub(crate) const fn has_receiver_property(&self) -> bool {
        self.has_receiver_property
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineValueKind {
    Unknown,
    Receiver,
}

fn read_kind(kinds: &[InlineValueKind], register: u16) -> Option<InlineValueKind> {
    kinds.get(usize::from(register)).copied()
}

fn write_kind(kinds: &mut [InlineValueKind], register: u16, kind: InlineValueKind) -> Option<()> {
    *kinds.get_mut(usize::from(register))? = kind;
    Some(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InlineAccess {
    reads: u32,
    dst: Option<u16>,
}

impl InlineAccess {
    const fn none() -> Self {
        Self {
            reads: 0,
            dst: None,
        }
    }

    fn read(reads: &[u16], register_count: u16) -> Option<Self> {
        Some(Self {
            reads: register_mask(reads, register_count)?,
            dst: None,
        })
    }

    fn write(dst: u16, register_count: u16) -> Option<Self> {
        validate_register(dst, register_count)?;
        Some(Self {
            reads: 0,
            dst: Some(dst),
        })
    }

    fn read_write(reads: &[u16], dst: u16, register_count: u16) -> Option<Self> {
        validate_register(dst, register_count)?;
        Some(Self {
            reads: register_mask(reads, register_count)?,
            dst: Some(dst),
        })
    }
}

fn validate_register(register: u16, register_count: u16) -> Option<()> {
    (register < register_count).then_some(())
}

fn register_mask(registers: &[u16], register_count: u16) -> Option<u32> {
    let mut mask = 0;
    for &register in registers {
        validate_register(register, register_count)?;
        mask |= node_bit(usize::from(register))?;
    }
    Some(mask)
}

fn node_bit(node: usize) -> Option<u32> {
    (node < u32::BITS as usize).then(|| 1u32 << node)
}

/// Build one static mapping from exact straight-line liveness.
///
/// The receiver is the pseudo node immediately after the virtual-register
/// range. Every access already encodes source reads before its optional
/// destination write.
fn build_scratch_layout(
    register_count: u16,
    argc: usize,
    accesses: &[InlineAccess],
) -> Option<InlineScratchLayout> {
    if register_count > INLINE_METHOD_MAX_REGISTERS || argc > usize::from(register_count) {
        return None;
    }
    let register_count_usize = usize::from(register_count);
    let receiver_node = register_count_usize;
    let node_count = receiver_node + 1;
    let mut interference = vec![0u32; node_count];
    let mut referenced = 0u32;
    let mut live = 0u32;

    for access in accesses.iter().rev() {
        let live_after = live;
        add_clique(&mut interference, live_after);
        referenced |= access.reads;
        if let Some(dst) = access.dst {
            validate_register(dst, register_count)?;
            let dst_bit = node_bit(usize::from(dst))?;
            referenced |= dst_bit;
            add_interference_set(&mut interference, usize::from(dst), live_after & !dst_bit);
            live = (live_after & !dst_bit) | access.reads;
        } else {
            live = live_after | access.reads;
        }
        add_clique(&mut interference, live);
    }
    let live_in = live;

    let mut nodes: Vec<usize> = (0..node_count)
        .filter(|&node| referenced & (1u32 << node) != 0)
        .collect();
    nodes.sort_by_key(|&node| {
        (
            Reverse((interference[node] & referenced).count_ones()),
            node,
        )
    });

    let mut node_slots = vec![None; node_count];
    let mut slot_count = 0u16;
    for node in nodes {
        let mut unavailable = 0u32;
        let mut neighbors = interference[node] & referenced;
        while neighbors != 0 {
            let neighbor = neighbors.trailing_zeros() as usize;
            neighbors &= neighbors - 1;
            if let Some(slot) = node_slots[neighbor] {
                unavailable |= 1u32 << InlineScratchSlot::index(slot);
            }
        }
        let mut slot = 0u16;
        while unavailable & (1u32 << slot) != 0 {
            slot = slot.checked_add(1)?;
        }
        node_slots[node] = Some(InlineScratchSlot(slot));
        slot_count = slot_count.max(slot.checked_add(1)?);
    }

    let register_slots = node_slots[..register_count_usize]
        .to_vec()
        .into_boxed_slice();
    let receiver_slot = node_slots[receiver_node];
    let mut entry_values = Vec::new();
    for argument in 0..argc {
        let register = u16::try_from(argument).ok()?;
        if live_in & node_bit(argument)? != 0 {
            entry_values.push(InlineEntryValue::Argument {
                argument: register,
                register,
                slot: register_slots[argument]?,
            });
        }
    }
    if live_in & node_bit(receiver_node)? != 0 {
        entry_values.push(InlineEntryValue::Receiver {
            slot: receiver_slot?,
        });
    }
    for register_index in argc..register_count_usize {
        if live_in & node_bit(register_index)? != 0 {
            let register = u16::try_from(register_index).ok()?;
            entry_values.push(InlineEntryValue::Undefined {
                register,
                slot: register_slots[register_index]?,
            });
        }
    }

    Some(InlineScratchLayout {
        register_slots,
        receiver_slot,
        entry_values: entry_values.into_boxed_slice(),
        slot_count,
    })
}

fn add_clique(interference: &mut [u32], members: u32) {
    let mut remaining = members;
    while remaining != 0 {
        let node = remaining.trailing_zeros() as usize;
        remaining &= remaining - 1;
        add_interference_set(interference, node, members & !(1u32 << node));
    }
}

fn add_interference_set(interference: &mut [u32], node: usize, mut others: u32) {
    while others != 0 {
        let other = others.trailing_zeros() as usize;
        others &= others - 1;
        interference[node] |= 1u32 << other;
        interference[other] |= 1u32 << node;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(reads: &[u16], dst: Option<u16>, register_count: u16) -> InlineAccess {
        match dst {
            Some(dst) => InlineAccess::read_write(reads, dst, register_count).unwrap(),
            None => InlineAccess::read(reads, register_count).unwrap(),
        }
    }

    #[test]
    fn sparse_virtual_window_compacts_to_live_slots() {
        let accesses = [
            access(&[], Some(17), 24),
            access(&[17], Some(23), 24),
            access(&[23], None, 24),
        ];
        let layout = build_scratch_layout(24, 1, &accesses).unwrap();

        assert_eq!(layout.slot_count(), 1);
        assert_eq!(layout.aligned_bytes(), 16);
        assert_eq!(layout.register_slot(0), None);
        assert_eq!(layout.register_slot(17), layout.register_slot(23));
        assert!(layout.entry_values().is_empty());
    }

    #[test]
    fn unused_and_overwritten_parameters_are_not_materialized() {
        let accesses = [
            access(&[], Some(0), 4),
            access(&[0], Some(2), 4),
            access(&[2], None, 4),
        ];
        let layout = build_scratch_layout(4, 2, &accesses).unwrap();

        assert!(
            !layout
                .entry_values()
                .iter()
                .any(|entry| matches!(entry, InlineEntryValue::Argument { .. }))
        );
        assert_eq!(layout.register_slot(1), None);
    }

    #[test]
    fn local_read_before_write_gets_mapped_undefined() {
        let accesses = [
            access(&[7], Some(8), 12),
            access(&[], Some(7), 12),
            access(&[8], None, 12),
        ];
        let layout = build_scratch_layout(12, 1, &accesses).unwrap();
        let slot = layout.register_slot(7).unwrap();

        assert!(
            layout
                .entry_values()
                .contains(&InlineEntryValue::Undefined { register: 7, slot })
        );
    }

    #[test]
    fn overwritten_binding_does_not_alias_live_snapshot() {
        // r8 snapshots the incoming r0, r0 is then overwritten, and both values
        // are read by the add. A static r0 mapping must therefore differ from r8
        // even though the move initially ends the old r0 value's lifetime.
        let accesses = [
            access(&[0], Some(8), 24),
            access(&[], Some(0), 24),
            access(&[8, 0], Some(23), 24),
            access(&[23], None, 24),
        ];
        let layout = build_scratch_layout(24, 1, &accesses).unwrap();

        assert_eq!(layout.slot_count(), 2);
        assert_ne!(layout.register_slot(0), layout.register_slot(8));
        assert!(matches!(
            layout.entry_values(),
            [InlineEntryValue::Argument {
                argument: 0,
                register: 0,
                ..
            }]
        ));
    }

    #[test]
    fn dead_destination_cannot_clobber_value_live_across_write() {
        let accesses = [
            access(&[1], Some(2), 4),
            access(&[], Some(3), 4),
            access(&[2], None, 4),
        ];
        let layout = build_scratch_layout(4, 2, &accesses).unwrap();

        assert_ne!(layout.register_slot(2), layout.register_slot(3));
    }

    #[test]
    fn receiver_can_coalesce_with_last_load_this_destination_deterministically() {
        let register_count = 8;
        let receiver_node = usize::from(register_count);
        let accesses = [
            InlineAccess {
                reads: node_bit(receiver_node).unwrap(),
                dst: Some(5),
            },
            access(&[5], None, register_count),
        ];
        let first = build_scratch_layout(register_count, 0, &accesses).unwrap();
        let second = build_scratch_layout(register_count, 0, &accesses).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.slot_count(), 1);
        assert_eq!(first.receiver_slot(), first.register_slot(5));
        assert!(matches!(
            first.entry_values(),
            [InlineEntryValue::Receiver { .. }]
        ));
    }

    #[test]
    fn simultaneous_argument_receiver_and_undefined_entries_do_not_alias() {
        let register_count = 4;
        let receiver_node = usize::from(register_count);
        let accesses = [
            InlineAccess {
                reads: node_bit(receiver_node).unwrap(),
                dst: Some(1),
            },
            access(&[0, 2], Some(3), register_count),
            access(&[1, 3], None, register_count),
        ];
        let layout = build_scratch_layout(register_count, 1, &accesses).unwrap();
        let argument_slot = layout.register_slot(0).unwrap();
        let receiver_slot = layout.receiver_slot().unwrap();
        let undefined_slot = layout.register_slot(2).unwrap();

        assert_ne!(argument_slot, receiver_slot);
        assert_ne!(argument_slot, undefined_slot);
        assert_ne!(receiver_slot, undefined_slot);
        assert_eq!(
            layout.entry_values(),
            [
                InlineEntryValue::Argument {
                    argument: 0,
                    register: 0,
                    slot: argument_slot,
                },
                InlineEntryValue::Receiver {
                    slot: receiver_slot,
                },
                InlineEntryValue::Undefined {
                    register: 2,
                    slot: undefined_slot,
                },
            ]
        );
    }

    #[test]
    fn two_source_destination_coalesces_after_both_reads() {
        let accesses = [access(&[0, 1], Some(2), 3), access(&[2], None, 3)];
        let layout = build_scratch_layout(3, 2, &accesses).unwrap();
        let lhs = layout.register_slot(0).unwrap();
        let rhs = layout.register_slot(1).unwrap();
        let dst = layout.register_slot(2).unwrap();

        assert_eq!(layout.slot_count(), 2);
        assert_ne!(lhs, rhs);
        assert!(dst == lhs || dst == rhs);
    }

    #[test]
    fn repeated_load_this_keeps_receiver_until_final_read() {
        let register_count = 2;
        let receiver_node = usize::from(register_count);
        let accesses = [
            InlineAccess {
                reads: node_bit(receiver_node).unwrap(),
                dst: Some(0),
            },
            InlineAccess {
                reads: node_bit(receiver_node).unwrap(),
                dst: Some(1),
            },
            access(&[0, 1], None, register_count),
        ];
        let layout = build_scratch_layout(register_count, 0, &accesses).unwrap();
        let receiver = layout.receiver_slot().unwrap();

        assert_eq!(layout.slot_count(), 2);
        assert_ne!(receiver, layout.register_slot(0).unwrap());
        assert_eq!(receiver, layout.register_slot(1).unwrap());
    }

    #[test]
    fn return_undefined_needs_no_scratch_storage() {
        let layout = build_scratch_layout(4, 0, &[InlineAccess::none()]).unwrap();

        assert_eq!(layout.slot_count(), 0);
        assert_eq!(layout.aligned_bytes(), 0);
        assert_eq!(layout.receiver_slot(), None);
        assert!(layout.entry_values().is_empty());
        assert!((0..4).all(|register| layout.register_slot(register).is_none()));
    }
}
