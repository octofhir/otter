//! Caller-owned storage for shared generated call linkage.
//!
//! # Contents
//! - [`StackLayout`] — checked offsets for the native header, control slots,
//!   upvalue spine, registers and actual arguments.
//!
//! # Invariants
//! - The native frame starts at SP; control slots and upvalues precede the
//!   tagged register/argument windows. Their offsets never depend on actual arity.
//! - Actual arguments immediately follow the complete register window, as
//!   required by the native frame root visitor. Register-base publication is
//!   authoritative; no consumer infers it from the native header size.
//! - Control slots and tagged windows are eight-byte aligned, capture offsets
//!   are four-byte aligned, and the complete reservation is sixteen-byte aligned.
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
    pub(super) register_base: u32,
    pub(super) incoming_base: u32,
    pub(super) incoming_count: u32,
    pub(super) upvalue_base: u32,
    pub(super) saved_x25: u32,
    pub(super) entry_addr: u32,
    pub(super) caller_frame: u32,
    pub(super) caller_code_object_id: u32,
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
        let upvalue_count = u32::from(target.plan.own_upvalue_count)
            .checked_add(u32::from(target.plan.inherited_upvalue_count))?;
        Self::for_windows(
            u32::from(target.plan.register_count),
            upvalue_count,
            incoming_count,
        )
    }

    fn for_windows(register_count: u32, upvalue_count: u32, incoming_count: u32) -> Option<Self> {
        let saved_x25 = NATIVE_FRAME_STACK_SIZE;
        let upvalue_base = saved_x25.checked_add(40)?;
        let upvalue_bytes = upvalue_count.checked_mul(4)?;
        let register_base = upvalue_base.checked_add(upvalue_bytes)?.checked_add(7)? & !7;
        let register_bytes = register_count.checked_mul(8)?;
        let incoming_base = register_base.checked_add(register_bytes)?;
        let frame_bytes = incoming_base
            .checked_add(incoming_count.checked_mul(8)?)?
            .checked_add(15)?
            & !15;
        (frame_bytes <= MAX_DIRECT_CALL_FRAME_BYTES).then_some(Self {
            register_base,
            incoming_base,
            incoming_count,
            upvalue_base,
            saved_x25,
            entry_addr: saved_x25 + 8,
            caller_frame: saved_x25 + 16,
            caller_code_object_id: saved_x25 + 24,
            target_cell: saved_x25 + 32,
            frame_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_arity_cannot_move_control_or_capture_storage() {
        for captures in [0, 1, 2, 7] {
            let empty = StackLayout::for_windows(19, captures, 0).unwrap();
            for actuals in [1, 2, 3, 16, 255] {
                let layout = StackLayout::for_windows(19, captures, actuals).unwrap();
                assert_eq!(layout.saved_x25, empty.saved_x25);
                assert_eq!(layout.entry_addr, empty.entry_addr);
                assert_eq!(layout.caller_frame, empty.caller_frame);
                assert_eq!(layout.caller_code_object_id, empty.caller_code_object_id);
                assert_eq!(layout.target_cell, empty.target_cell);
                assert_eq!(layout.upvalue_base, empty.upvalue_base);
                assert_eq!(layout.register_base, empty.register_base);
                assert_eq!(layout.incoming_base, layout.register_base + 19 * 8);
                assert!(layout.register_base >= layout.upvalue_base + captures * 4);
                assert!(layout.frame_bytes >= layout.incoming_base + actuals * 8);
                assert_eq!(layout.register_base % 8, 0);
                assert_eq!(layout.frame_bytes % 16, 0);
            }
        }
    }

    #[test]
    fn oversized_or_overflowing_windows_are_rejected() {
        assert!(StackLayout::for_windows(u32::MAX, 0, 0).is_none());
        assert!(StackLayout::for_windows(0, u32::MAX, 0).is_none());
        assert!(StackLayout::for_windows(0, 0, u32::MAX).is_none());
        assert!(StackLayout::for_windows(0, 0, MAX_DIRECT_CALL_FRAME_BYTES / 8).is_none());
    }
}
