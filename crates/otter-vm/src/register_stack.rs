//! Segmented native register-window storage and GC publication.
//!
//! This module owns the register arena used by compiled call chains. Machine
//! code sees only each published activation's window pointer and count; the
//! interpreter retains ownership of allocation, reclamation, and root tracing.
//!
//! # Contents
//! - [`RegisterStack`] — lazily segmented tagged-slot arena.
//! - Window allocation and stack-discipline reclamation.
//! - Precise tracing of the published live prefix.
//!
//! # Invariants
//! - Each segment owns a heap-stable buffer that never grows while active.
//! - Moving the segment descriptor vector never invalidates a live window.
//! - Only used prefixes of active segments are published to GC.
//! - New windows are initialized to `undefined` before publication.
//! - Reclamation is LIFO and only lowers the logical top, so duplicate cleanup
//!   cannot expose stale slots as roots.
//!
//! # See also
//! - [`crate::frame_state::Frame`]
//! - [`crate::activation_stack::ActivationStack`]
//! - [`crate::native_abi::NativeFrame`]

use crate::{Value, VmError};
use otter_gc::{GcHeap, raw::RawGc};

/// Maximum tagged slots in one native call-chain arena.
pub(crate) const REGISTER_STACK_CAPACITY: usize = 512 * 1024;
/// Default storage granularity for ordinary native frames.
const REGISTER_SEGMENT_SLOTS: usize = 4 * 1024;
/// C-layout descriptor for one contiguous tagged register window.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterWindow {
    base: *mut Value,
    len: u32,
    stack_base: u32,
}

impl RegisterWindow {
    pub(crate) fn attached(base: *mut Value, len: usize, stack_base: u32) -> Self {
        Self {
            base,
            len: u32::try_from(len).expect("register window length exceeds u32"),
            stack_base,
        }
    }

    /// Base of initialized tagged slots.
    #[must_use]
    pub fn as_mut_ptr(self) -> *mut Value {
        self.base
    }

    /// Number of initialized tagged slots.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len as usize
    }

    /// Whether the window contains no tagged slots.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Slot offset in [`RegisterStack`]. Every active frame window is attached.
    #[must_use]
    pub const fn stack_base(self) -> u32 {
        self.stack_base
    }
}

impl std::ops::Deref for RegisterWindow {
    type Target = [Value];

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: active windows point at initialized slots in a heap-stable
        // RegisterStack segment and are released only after the owning frame
        // leaves the active stack.
        unsafe { std::slice::from_raw_parts(self.base, self.len()) }
    }
}

impl std::ops::DerefMut for RegisterWindow {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: Frame owns exclusive mutable access to its attached window.
        unsafe { std::slice::from_raw_parts_mut(self.base, self.len()) }
    }
}

const _: [(); 16] = [(); std::mem::size_of::<RegisterWindow>()];
const _: [(); 8] = [(); std::mem::align_of::<RegisterWindow>()];
const _: [(); 0] = [(); std::mem::offset_of!(RegisterWindow, base)];
const _: [(); 8] = [(); std::mem::offset_of!(RegisterWindow, len)];
const _: [(); 12] = [(); std::mem::offset_of!(RegisterWindow, stack_base)];

/// VM-owned segmented register arena for native call chains.
///
/// Segments are allocated on first use and retained after LIFO release. Each
/// inner `Vec` reserves its final capacity before publication, so the outer
/// descriptor vector may grow without moving any published tagged slots.
#[derive(Debug, Default)]
pub(crate) struct RegisterStack {
    segments: Vec<RegisterSegment>,
    active_segments: usize,
    top: usize,
}

#[derive(Debug)]
struct RegisterSegment {
    slots: Vec<Value>,
    logical_base: usize,
    used: usize,
}

impl RegisterSegment {
    fn new(minimum_capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(REGISTER_SEGMENT_SLOTS.max(minimum_capacity)),
            logical_base: 0,
            used: 0,
        }
    }

    #[inline]
    fn remaining(&self) -> usize {
        self.slots.capacity() - self.used
    }

    fn activate(&mut self, logical_base: usize) {
        debug_assert_eq!(self.used, 0);
        self.logical_base = logical_base;
    }

    /// Initialize one suffix without changing the segment's allocation.
    fn allocate(&mut self, count: usize) -> *mut Value {
        debug_assert!(count <= self.remaining());
        let base = self.used;
        let end = base + count;
        let initialized = self.slots.len();
        if end > initialized {
            self.slots.resize(end, Value::undefined());
        }
        if base < initialized {
            self.slots[base..end.min(initialized)].fill(Value::undefined());
        }
        self.used = end;
        // SAFETY: `base <= end <= capacity`; resize above initializes the live
        // range, and this segment cannot grow while it is active.
        unsafe { self.slots.as_mut_ptr().add(base) }
    }
}

/// Rollback guard for a sequence that may allocate unpublished windows.
///
/// The raw pointer avoids borrowing the whole interpreter while frame setup
/// performs other VM operations. The interpreter is mutably borrowed for the
/// guard's lexical scope and therefore cannot move; drop only lowers `top`.
pub(crate) struct RegisterStackCheckpoint {
    stack: *mut RegisterStack,
    top: usize,
    committed: bool,
}

impl RegisterStackCheckpoint {
    fn new(stack: &mut RegisterStack) -> Self {
        Self {
            stack,
            top: stack.checkpoint(),
            committed: false,
        }
    }

    /// Transfer ownership of subsequently allocated windows to active frames.
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for RegisterStackCheckpoint {
    fn drop(&mut self) {
        if !self.committed {
            // SAFETY: constructed from the current interpreter's stable
            // RegisterStack field; no reference is retained or exposed.
            unsafe { (*self.stack).restore(self.top) };
        }
    }
}

impl RegisterStack {
    /// Empty lazy arena.
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            segments: Vec::new(),
            active_segments: 0,
            top: 0,
        }
    }

    pub(crate) fn rollback_checkpoint(&mut self) -> RegisterStackCheckpoint {
        RegisterStackCheckpoint::new(self)
    }

    fn activate_segment(&mut self, minimum_capacity: usize) -> &mut RegisterSegment {
        let insertion = self.active_segments;
        let reusable = self.segments[insertion..]
            .iter()
            .position(|segment| segment.slots.capacity() >= minimum_capacity)
            .map(|offset| insertion + offset);
        let selected = if let Some(selected) = reusable {
            selected
        } else {
            self.segments.push(RegisterSegment::new(minimum_capacity));
            self.segments.len() - 1
        };
        self.segments.swap(insertion, selected);
        self.active_segments += 1;
        let segment = &mut self.segments[insertion];
        segment.activate(self.top);
        segment
    }

    /// Reserve and initialize one contiguous tagged window.
    pub(crate) fn allocate(&mut self, count: usize) -> Result<RegisterWindow, VmError> {
        let base = self.top;
        let end = base
            .checked_add(count)
            .filter(|&end| end <= REGISTER_STACK_CAPACITY)
            .ok_or(VmError::StackOverflow {
                limit: REGISTER_STACK_CAPACITY as u32,
            })?;
        if count == 0 {
            let pointer = self
                .segments
                .get_mut(self.active_segments.wrapping_sub(1))
                .map_or_else(
                    || std::ptr::NonNull::<Value>::dangling().as_ptr(),
                    |segment| {
                        // SAFETY: an active segment owns at least `used`
                        // initialized slots; a one-past pointer is valid for a
                        // zero-length slice.
                        unsafe { segment.slots.as_mut_ptr().add(segment.used) }
                    },
                );
            return Ok(RegisterWindow::attached(pointer, 0, base as u32));
        }

        let needs_segment = self.active_segments == 0
            || self.segments[self.active_segments - 1].remaining() < count;
        if needs_segment {
            self.activate_segment(count);
        }
        let pointer = self.segments[self.active_segments - 1].allocate(count);
        self.top = end;
        Ok(RegisterWindow::attached(pointer, count, base as u32))
    }

    /// Release a window and every younger window.
    pub(crate) fn release(&mut self, base: u32) {
        self.lower_top(self.top.min(base as usize));
    }

    /// Save the live cursor for nested-dispatch restoration.
    #[must_use]
    pub(crate) const fn checkpoint(&self) -> usize {
        self.top
    }

    /// Release every window allocated after `checkpoint` without ever raising
    /// the live cursor.
    pub(crate) fn restore(&mut self, checkpoint: usize) {
        self.lower_top(self.top.min(checkpoint));
    }

    fn lower_top(&mut self, new_top: usize) {
        if new_top == self.top {
            return;
        }
        debug_assert!(new_top < self.top);
        while self.active_segments != 0 {
            let index = self.active_segments - 1;
            let logical_base = self.segments[index].logical_base;
            if new_top > logical_base {
                let segment = &mut self.segments[index];
                debug_assert!(new_top <= logical_base + segment.used);
                segment.used = new_top - logical_base;
                break;
            }
            self.segments[index].used = 0;
            self.active_segments -= 1;
        }
        self.top = new_top;
    }

    /// Describe the youngest already-published window without copying or
    /// changing the live cursor. Used to materialize interpreter frames after
    /// a frameless compiled call bails.
    pub(crate) fn top_window(&mut self, count: usize) -> Result<RegisterWindow, VmError> {
        let base = self.top.checked_sub(count).ok_or(VmError::InvalidOperand)?;
        if count == 0 {
            let pointer = self
                .segments
                .get_mut(self.active_segments.wrapping_sub(1))
                .map_or_else(
                    || std::ptr::NonNull::<Value>::dangling().as_ptr(),
                    |segment| {
                        // SAFETY: one-past the initialized prefix is valid for
                        // a zero-length slice.
                        unsafe { segment.slots.as_mut_ptr().add(segment.used) }
                    },
                );
            return Ok(RegisterWindow::attached(
                pointer,
                0,
                u32::try_from(base).map_err(|_| VmError::InvalidOperand)?,
            ));
        }
        let segment = self
            .segments
            .get_mut(
                self.active_segments
                    .checked_sub(1)
                    .ok_or(VmError::InvalidOperand)?,
            )
            .ok_or(VmError::InvalidOperand)?;
        if base < segment.logical_base || self.top != segment.logical_base + segment.used {
            return Err(VmError::InvalidOperand);
        }
        let offset = base - segment.logical_base;
        if offset.checked_add(count) != Some(segment.used) {
            return Err(VmError::InvalidOperand);
        }
        // SAFETY: the requested top suffix is within the active initialized
        // prefix and the segment allocation cannot move while active.
        let pointer = unsafe { segment.slots.as_mut_ptr().add(offset) };
        Ok(RegisterWindow::attached(
            pointer,
            count,
            u32::try_from(base).map_err(|_| VmError::InvalidOperand)?,
        ))
    }

    /// Trace the precisely published tagged prefix.
    pub(crate) fn trace(&self, heap: &GcHeap, visitor: &mut dyn FnMut(*mut RawGc)) {
        if std::env::var_os("OTTER_GC_VERIFY").is_some_and(|value| value != "0") {
            self.verify_roots(heap);
        }
        for segment in &self.segments[..self.active_segments] {
            for value in &segment.slots[..segment.used] {
                value.trace_value_slots(visitor);
            }
        }
    }

    #[cold]
    fn verify_roots(&self, heap: &GcHeap) {
        for segment in &self.segments[..self.active_segments] {
            let base = segment.slots.as_ptr();
            for (offset, value) in segment.slots[..segment.used].iter().enumerate() {
                if let Some((size, tag, forwarded)) = value.debug_gc_target_header(heap)
                    && !forwarded
                    && (size == 0 || size > (1_u32 << 20) || tag == 0)
                {
                    let index = segment.logical_base + offset;
                    eprintln!(
                        "OTTER_GC_VERIFY: stale register-stack slot idx={index} top={} segment_base={base:p} target_size={size} target_tag={tag}",
                        self.top
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_is_unallocated_before_first_non_empty_window() {
        let mut stack = RegisterStack::new();
        assert!(stack.segments.is_empty());
        assert!(stack.allocate(0).unwrap().is_empty());
        assert!(stack.segments.is_empty());
    }

    #[test]
    fn first_small_window_reserves_only_one_segment() {
        let mut stack = RegisterStack::new();
        let window = stack.allocate(4).unwrap();
        assert_eq!(window.stack_base(), 0);
        assert_eq!(stack.segments.len(), 1);
        assert!(stack.segments[0].slots.capacity() >= REGISTER_SEGMENT_SLOTS);
        assert!(stack.segments[0].slots.capacity() < REGISTER_STACK_CAPACITY);
        assert_eq!(stack.segments[0].slots.len(), 4);
        assert!(window.iter().all(|value| *value == Value::undefined()));
    }

    #[test]
    fn live_pointer_survives_outer_segment_growth() {
        let mut stack = RegisterStack::new();
        let mut first = stack.allocate(1).unwrap();
        first[0] = Value::number_i32(41);
        let first_ptr = first.as_mut_ptr();

        for _ in 0..32 {
            stack.allocate(REGISTER_SEGMENT_SLOTS).unwrap();
        }

        assert!(stack.segments.capacity() > 1);
        assert_eq!(first.as_mut_ptr(), first_ptr);
        assert_eq!(first[0], Value::number_i32(41));
    }

    #[test]
    fn lifo_release_reuses_segment_and_traces_only_live_prefixes() {
        let mut heap = GcHeap::new().expect("gc heap");
        let map = crate::collections::alloc_map(&mut heap).expect("map");
        let root = Value::map(map);
        let mut stack = RegisterStack::new();
        let mut first = stack.allocate(REGISTER_SEGMENT_SLOTS).unwrap();
        first[0] = root;
        let mut second = stack.allocate(2).unwrap();
        second[0] = root;
        second[1] = root;
        let second_ptr = second.as_mut_ptr();

        let mut roots = 0;
        stack.trace(&heap, &mut |_| roots += 1);
        assert_eq!(roots, 3);

        stack.release(second.stack_base());
        let mut roots = 0;
        stack.trace(&heap, &mut |_| roots += 1);
        assert_eq!(roots, 1);

        let reused = stack.allocate(2).unwrap();
        assert_eq!(reused.as_mut_ptr(), second_ptr);
        assert!(reused.iter().all(|value| *value == Value::undefined()));
    }

    #[test]
    fn restore_never_republishes_released_slots() {
        let mut stack = RegisterStack::new();
        stack.allocate(5).unwrap();
        let checkpoint = stack.checkpoint();
        stack.allocate(REGISTER_SEGMENT_SLOTS).unwrap();
        stack.restore(checkpoint);
        assert_eq!(stack.checkpoint(), 5);

        stack.release(2);
        stack.restore(checkpoint);
        assert_eq!(stack.checkpoint(), 2);
    }

    #[test]
    fn top_window_resolves_only_the_youngest_segment() {
        let mut stack = RegisterStack::new();
        stack.allocate(REGISTER_SEGMENT_SLOTS).unwrap();
        let youngest = stack.allocate(3).unwrap();

        assert_eq!(stack.top_window(3).unwrap(), youngest);
        assert!(matches!(
            stack.top_window(REGISTER_SEGMENT_SLOTS + 3),
            Err(VmError::InvalidOperand)
        ));
        assert!(stack.top_window(0).unwrap().is_empty());
    }

    #[test]
    fn global_slot_limit_is_enforced_across_segments() {
        let mut stack = RegisterStack::new();
        stack.allocate(REGISTER_STACK_CAPACITY - 1).unwrap();
        stack.allocate(1).unwrap();
        assert!(matches!(
            stack.allocate(1),
            Err(VmError::StackOverflow { limit }) if limit == REGISTER_STACK_CAPACITY as u32
        ));
    }
}
