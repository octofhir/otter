//! Stable native register-window storage and GC publication.
//!
//! This module owns the register arena used by compiled call chains. Machine
//! code sees only the current arena base, live-slot cursor, and capacity; the
//! interpreter retains ownership of allocation, reclamation, and root tracing.
//!
//! # Contents
//! - [`RegisterStack`] — reservation-stable tagged-slot arena.
//! - Window allocation and stack-discipline reclamation.
//! - Precise tracing of the published live prefix.
//!
//! # Invariants
//! - The backing allocation never moves while a window is live.
//! - Only `slots[..top]` is published to GC.
//! - New windows are initialized to `undefined` before publication.
//! - Reclamation only lowers `top`, so duplicate cleanup cannot expose stale
//!   slots as roots.
//!
//! # See also
//! - [`crate::frame_state::FrameRegisters`]
//! - [`crate::holt_stack::HoltStack`]
//! - [`crate::native_abi::NativeFrame`]

use otter_gc::{GcHeap, raw::RawGc};
use smallvec::SmallVec;

use crate::{Value, VmError};

/// Maximum tagged slots in one native call-chain arena.
pub(crate) const REGISTER_STACK_CAPACITY: usize = 512 * 1024;

/// VM-owned, reservation-stable register arena for native call chains.
#[derive(Debug, Default)]
pub(crate) struct RegisterStack {
    slots: Vec<Value>,
    top: usize,
}

impl RegisterStack {
    /// Empty lazy arena.
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            slots: Vec::new(),
            top: 0,
        }
    }

    fn ensure_allocated(&mut self) {
        if self.slots.is_empty() {
            self.slots = vec![Value::undefined(); REGISTER_STACK_CAPACITY];
        }
    }

    /// Stable base address exposed to generated code.
    pub(crate) fn base_ptr(&mut self) -> *mut u64 {
        self.ensure_allocated();
        self.slots.as_mut_ptr().cast::<u64>()
    }

    /// Address of the live-slot cursor exposed to generated code.
    pub(crate) fn top_ptr(&mut self) -> *mut usize {
        &mut self.top
    }

    /// Reserve and initialize one contiguous tagged window.
    pub(crate) fn allocate(&mut self, count: usize) -> Result<(*mut Value, u32), VmError> {
        self.ensure_allocated();
        let base = self.top;
        let end = base
            .checked_add(count)
            .filter(|&end| end <= REGISTER_STACK_CAPACITY)
            .ok_or(VmError::StackOverflow {
                limit: REGISTER_STACK_CAPACITY as u32,
            })?;
        let window = &mut self.slots[base..end];
        window.fill(Value::undefined());
        self.top = end;
        Ok((window.as_mut_ptr(), base as u32))
    }

    /// Release a window and every younger window.
    pub(crate) fn release(&mut self, base: u32) {
        self.top = self.top.min(base as usize);
    }

    /// Save the live cursor for nested-dispatch restoration.
    #[must_use]
    pub(crate) const fn checkpoint(&self) -> usize {
        self.top
    }

    /// Release every window allocated after `checkpoint` without ever raising
    /// the live cursor.
    pub(crate) fn restore(&mut self, checkpoint: usize) {
        self.top = self.top.min(checkpoint);
    }

    /// Copy and release the youngest window when compiled code bails before it
    /// can publish an interpreter frame.
    pub(crate) fn take_top(&mut self, count: usize) -> Result<SmallVec<[Value; 8]>, VmError> {
        let base = self.top.checked_sub(count).ok_or(VmError::InvalidOperand)?;
        let mut values = SmallVec::with_capacity(count);
        values.extend_from_slice(&self.slots[base..self.top]);
        self.top = base;
        Ok(values)
    }

    /// Trace the precisely published tagged prefix.
    pub(crate) fn trace(&self, heap: &GcHeap, visitor: &mut dyn FnMut(*mut RawGc)) {
        if std::env::var_os("OTTER_GC_VERIFY").is_some_and(|value| value != "0") {
            self.verify_roots(heap);
        }
        for value in &self.slots[..self.top] {
            value.trace_value_slots(visitor);
        }
    }

    #[cold]
    fn verify_roots(&self, heap: &GcHeap) {
        let base = self.slots.as_ptr();
        for (index, value) in self.slots[..self.top].iter().enumerate() {
            if let Some((size, tag, forwarded)) = value.debug_gc_target_header(heap)
                && !forwarded
                && (size == 0 || size > (1_u32 << 20) || tag == 0)
            {
                eprintln!(
                    "OTTER_GC_VERIFY: stale register-stack slot idx={index} top={} base={base:p} target_size={size} target_tag={tag}",
                    self.top
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_keeps_base_stable_and_initializes_slots() {
        let mut stack = RegisterStack::new();
        let base = stack.base_ptr();
        let (first, first_offset) = stack.allocate(4).unwrap();
        let (second, second_offset) = stack.allocate(3).unwrap();

        assert_eq!(base, first.cast::<u64>());
        assert_eq!(first_offset, 0);
        assert_eq!(second_offset, 4);
        assert_eq!(stack.base_ptr(), base);
        // SAFETY: both windows are live and initialized by `allocate`.
        unsafe {
            assert_eq!(*first.add(3), Value::undefined());
            assert_eq!(*second.add(2), Value::undefined());
        }
    }

    #[test]
    fn restore_never_republishes_released_slots() {
        let mut stack = RegisterStack::new();
        stack.allocate(5).unwrap();
        let checkpoint = stack.checkpoint();
        stack.allocate(7).unwrap();
        stack.restore(checkpoint);
        assert_eq!(stack.checkpoint(), 5);

        stack.release(2);
        stack.restore(checkpoint);
        assert_eq!(stack.checkpoint(), 2);
    }

    #[test]
    fn take_top_copies_and_releases_bailed_window() {
        let mut stack = RegisterStack::new();
        let (window, _) = stack.allocate(2).unwrap();
        // SAFETY: the window is live and exclusively owned by this test.
        unsafe {
            *window = Value::number_i32(7);
            *window.add(1) = Value::number_i32(11);
        }

        let values = stack.take_top(2).unwrap();
        assert_eq!(
            values.as_slice(),
            [Value::number_i32(7), Value::number_i32(11)]
        );
        assert_eq!(stack.checkpoint(), 0);
        assert!(matches!(stack.take_top(1), Err(VmError::InvalidOperand)));
    }
}
