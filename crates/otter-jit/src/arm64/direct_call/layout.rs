//! Caller-owned storage for shared generated call linkage.
//!
//! # Contents
//! - [`StackLayout`] — checked offsets for the native header, control slots,
//!   registers and actual arguments.
//!
//! # Invariants
//! - The native frame starts at SP; control slots precede the tagged
//!   register/argument windows. Their offsets never depend on actual arity.
//! - Actual arguments immediately follow the complete register window, as
//!   required by the native frame root visitor. Register-base publication is
//!   authoritative; no consumer infers it from the native header size.
//! - The frame carries no binding storage: a callee reaches its captured
//!   bindings through its SELF closure's context.
//! - Control slots and tagged windows are eight-byte aligned, and the complete
//!   reservation is sixteen-byte aligned.
//! - Forwarded actual windows keep their runtime reservation and the admitted
//!   call plan in fixed control slots. Their complete size is bounded before
//!   SP changes and root publication.
//! - The caller link and depth live in the native header, not in control slots.
//! - Every size operation is checked before applying the generated-call bound.
//!
//! # See also
//! - [`super::emit_direct_call_with_access`] — initialization and publication.
//! - `otter-vm/src/active_frame.rs` — checked native window access and tracing.

use otter_vm::JitDirectCallee;

use super::MAX_DIRECT_CALL_FRAME_BYTES;
use crate::entry::NATIVE_FRAME_STACK_SIZE;

#[derive(Debug, Clone, Copy)]
pub(super) struct StackLayout {
    pub(super) allocation_size: Option<u32>,
    pub(super) plan: Option<u32>,
    pub(super) register_base: u32,
    pub(super) incoming_base: u32,
    pub(super) incoming_count: u32,
    pub(super) saved_x25: u32,
    pub(super) entry_addr: u32,
    pub(super) target_cell: u32,
    pub(super) frame_bytes: u32,
}

impl StackLayout {
    /// Reserve all actual arguments only when the target consumes that window.
    pub(super) fn for_site(target: &JitDirectCallee, argument_count: u32) -> Option<Self> {
        target
            .plan
            .generated_stack_frame_bytes
            .filter(|bytes| *bytes != 0)?;
        let incoming_count = if target.plan.needs_incoming_arguments {
            argument_count
        } else {
            0
        };
        Self::for_windows(u32::from(target.plan.register_count), incoming_count, false)
    }

    /// Minimum shared prefix for a runtime-selected target. Window offsets are
    /// computed from its admitted metadata; completion consumes only controls.
    pub(super) fn dynamic_prefix() -> Self {
        Self::for_windows(0, 0, true).expect("native control prefix fits the call bound")
    }

    pub(super) fn for_forward(target: &JitDirectCallee) -> Option<Self> {
        target
            .plan
            .generated_stack_frame_bytes
            .filter(|bytes| *bytes != 0)?;
        Self::for_windows(
            u32::from(target.plan.register_count),
            0,
            target.plan.needs_incoming_arguments,
        )
    }

    fn for_windows(register_count: u32, incoming_count: u32, dynamic: bool) -> Option<Self> {
        let saved_x25 = NATIVE_FRAME_STACK_SIZE;
        let control_end = saved_x25.checked_add(24)?;
        let plan = dynamic.then_some(control_end);
        let allocation_size = dynamic.then_some(control_end + 8);
        let register_base = control_end.checked_add(if dynamic { 16 } else { 0 })?;
        let register_bytes = register_count.checked_mul(8)?;
        let incoming_base = register_base.checked_add(register_bytes)?;
        let frame_bytes = incoming_base
            .checked_add(incoming_count.checked_mul(8)?)?
            .checked_add(15)?
            & !15;
        (frame_bytes <= MAX_DIRECT_CALL_FRAME_BYTES).then_some(Self {
            allocation_size,
            plan,
            register_base,
            incoming_base,
            incoming_count,
            saved_x25,
            entry_addr: saved_x25 + 8,
            target_cell: saved_x25 + 16,
            frame_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_arity_cannot_move_control_or_register_storage() {
        for dynamic in [false, true] {
            let empty = StackLayout::for_windows(19, 0, dynamic).unwrap();
            for actuals in [1, 2, 3, 16, 255] {
                let layout = StackLayout::for_windows(19, actuals, dynamic).unwrap();
                assert_eq!(layout.allocation_size, empty.allocation_size);
                assert_eq!(layout.allocation_size.is_some(), dynamic);
                assert_eq!(layout.saved_x25, empty.saved_x25);
                assert_eq!(layout.entry_addr, empty.entry_addr);
                assert_eq!(layout.plan, empty.plan);
                assert_eq!(layout.target_cell, empty.target_cell);
                assert_eq!(layout.register_base, empty.register_base);
                assert_eq!(layout.incoming_base, layout.register_base + 19 * 8);
                assert!(layout.register_base >= layout.target_cell + 8);
                assert!(layout.frame_bytes >= layout.incoming_base + actuals * 8);
                assert_eq!(layout.register_base % 8, 0);
                assert_eq!(layout.frame_bytes % 16, 0);
            }
        }
    }

    #[test]
    fn oversized_or_overflowing_windows_are_rejected() {
        assert!(StackLayout::for_windows(u32::MAX, 0, false).is_none());
        assert!(StackLayout::for_windows(0, u32::MAX, false).is_none());
        assert!(StackLayout::for_windows(0, MAX_DIRECT_CALL_FRAME_BYTES / 8, false).is_none());
    }
}
