//! HoltStack — contiguous, reservation-stable execution-frame stack.
//!
//! Before this slice the interpreter held its call frames in a
//! `SmallVec<[Frame; 8]>`. That works for a pure interpreter but is the wrong
//! substrate for machine-code calls and optimizer deopt: a buffer that
//! **reallocates and moves every live frame** when it outgrows its capacity is
//! unusable once a compiled callee holds a raw pointer to its caller's frame —
//! and to the register slots inside it — across a re-entrant call.
//!
//! `HoltStack` is that substrate. It is a single contiguous buffer, so frame
//! access is one indirection and `stack[i]` is O(1) — exactly the cost the
//! legacy `SmallVec` paid. Stability comes from **reservation, not
//! segmentation**: *every* `HoltStack` reserves [`crate::DEFAULT_MAX_STACK_DEPTH`]
//! frames up front ([`HoltStack::new`]). The VM throws a catchable stack-overflow
//! before the live frame count could exceed that bound, so the backing buffer
//! never reallocates and every `&Frame` it has handed out stays put for the
//! stack's lifetime. There is one stable behavior — no inline / non-reserved
//! mode. Short-lived re-entry stacks (Array callbacks, eval, generator
//! prologues) are recycled through the interpreter's stack pool
//! (`Interpreter::draw_stack` / `return_stack`) so they cost no per-call
//! reallocation, and a compiled callee can append its frame directly onto the
//! caller's stack and re-enter without the caller's in-register frame pointer
//! ever moving.
//!
//! This **is** the interpreter execution stack: slice 1b replaced
//! `SmallVec<[Frame; 8]>` with `HoltStack` at every call site and rewired the
//! GC frame-roots provider (`trace_active_frame_roots`) onto it. There is no
//! legacy fallback.
//!
//! # Contents
//! - [`HoltStack`] — the frame stack and its stack-discipline API.
//! - [`HoltCallReservation`] — unpublished frame owner for two-phase call-frame
//!   construction.
//! - [`HoltFrameDesc`] / [`HoltValueSlots`] — stable frame index and value-slot
//!   pointer metadata consumed by JIT call-entry work.
//!
//! # Invariants
//! - A [`HoltStack`] never exceeds its reserved capacity (the VM's
//!   stack-overflow guard fires first), so its buffer never reallocates and
//!   live-frame addresses are stable.
//! - GC tracing visits every live frame exactly once, in push order.
//!
//! # See also
//! - [`crate::frame_state::Frame`] — the frame payload and `trace_frame_slots`.
//! - [`crate::cold_frame`] — cold side records, traced separately by the
//!   integration just as they were for the `SmallVec` stack.

use smallvec::SmallVec;

use crate::{Value, frame_state::Frame};

/// Raw metadata for a frame's traced value slots.
///
/// This is intentionally just pointer + length. Safe Rust code should continue
/// indexing through [`HoltStack`]; emitted code and VM-side JIT ABI helpers use
/// this descriptor to address the register window without learning the Rust
/// `Frame` layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoltValueSlots {
    ptr: *mut Value,
    len: usize,
}

impl HoltValueSlots {
    /// Raw pointer to the first value slot.
    #[inline]
    #[must_use]
    pub fn as_mut_ptr(self) -> *mut Value {
        self.ptr
    }

    /// Number of value slots in this frame window.
    #[inline]
    #[must_use]
    pub fn len(self) -> usize {
        self.len
    }

    /// `true` when the frame has no value slots.
    #[inline]
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Published frame descriptor for a live frame on a [`HoltStack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoltFrameDesc {
    index: usize,
    value_slots: HoltValueSlots,
}

impl HoltFrameDesc {
    /// Frame index in the owning [`HoltStack`].
    #[inline]
    #[must_use]
    pub fn index(self) -> usize {
        self.index
    }

    /// Raw value-slot metadata for the frame's register window.
    #[inline]
    #[must_use]
    pub fn value_slots(self) -> HoltValueSlots {
        self.value_slots
    }
}

/// Unpublished call-frame reservation.
///
/// The frame is fully owned here and is not visible to GC frame-root tracing
/// until [`Self::publish`] moves it onto a [`HoltStack`]. This is the substrate
/// direct JIT calls need: Rust initializes header/cold/upvalue state first,
/// then emitted code can fill value slots through a descriptor only after the
/// VM publishes a fully initialized frame shape.
#[derive(Debug)]
pub struct HoltCallReservation {
    frame: Frame,
}

impl HoltCallReservation {
    /// Create a reservation around a fully allocated but unpublished frame.
    #[inline]
    #[must_use]
    pub fn from_frame(frame: Frame) -> Self {
        Self { frame }
    }

    /// Mutate the unpublished frame before it becomes GC-visible.
    #[inline]
    #[must_use]
    pub fn frame_mut(&mut self) -> &mut Frame {
        &mut self.frame
    }

    /// Raw metadata for the unpublished frame's value slots.
    #[inline]
    #[must_use]
    pub fn value_slots(&mut self) -> HoltValueSlots {
        HoltValueSlots {
            ptr: self.frame.registers.as_mut_ptr(),
            len: self.frame.registers.len(),
        }
    }

    /// Publish the frame onto `stack`, returning the stable descriptor for the
    /// now-live frame.
    #[inline]
    pub fn publish(self, stack: &mut HoltStack) -> HoltFrameDesc {
        stack.publish_call_reservation(self)
    }
}

/// `SmallVec` inline threshold. Immaterial to behavior — every `HoltStack`
/// reserves [`crate::DEFAULT_MAX_STACK_DEPTH`] up front, so storage always lives
/// in the heap buffer; this only fixes the `SmallVec`'s spilled layout, which the
/// JIT reentry bridge's `<*mut JitFrameStack>::cast` reinterprets.
const INLINE_FRAMES: usize = 8;

/// Reservation-stable stack of interpreter call [`Frame`]s.
///
/// Frame access is one indirection and `stack[i]` is O(1) — exactly the cost the
/// legacy `SmallVec<[Frame; 8]>` paid. Stability comes from **reservation**:
/// every stack reserves [`crate::DEFAULT_MAX_STACK_DEPTH`] frames in one heap
/// buffer up front ([`Self::new`]), and the VM throws a catchable stack-overflow
/// before the live frame count could exceed it — so that buffer never
/// reallocates and every `&Frame` it hands out stays put. There is no inline /
/// non-reserved mode; short-lived re-entry stacks are pooled and reused.
///
/// Same stack-discipline surface as the legacy stack (`push` / `pop` / `last` /
/// `last_mut` / `len` / `is_empty` / `get` / `get_mut` / `truncate` / `clear` /
/// `iter` / `iter_mut`) plus `Index` / `IndexMut`.
///
/// `#[repr(transparent)]` over the `SmallVec`: `HoltStack` has identical layout
/// and ABI to its storage, so the JIT reentry bridge's
/// `<*mut JitFrameStack>::cast(stack)` is a sound, zero-cost reinterpret and the
/// optimizer treats the newtype exactly as the bare `SmallVec` it once was.
#[repr(transparent)]
#[derive(Debug)]
pub struct HoltStack {
    frames: SmallVec<[Frame; INLINE_FRAMES]>,
}

impl Default for HoltStack {
    /// Reserved, like [`Self::new`] — the only behavior, so a defaulted stack is
    /// stable too.
    fn default() -> Self {
        Self::new()
    }
}

impl HoltStack {
    /// A new, empty stack pre-reserved for a full dispatch run.
    ///
    /// Every `HoltStack` reserves [`crate::DEFAULT_MAX_STACK_DEPTH`] frames up
    /// front, spilling storage to a single heap buffer. The VM throws a catchable
    /// stack-overflow before the live frame count could exceed that bound, so the
    /// buffer never reallocates and every `&Frame` it hands out keeps a stable
    /// address — the property compiled callees rely on to append a callee frame
    /// onto the caller's own stack and re-enter without dangling the caller's
    /// in-register frame pointer. There is no inline / non-reserved mode: one
    /// stable behavior, with short-lived re-entry stacks recycled through the
    /// interpreter's stack pool rather than reallocated per call.
    #[must_use]
    pub fn new() -> Self {
        Self {
            frames: SmallVec::with_capacity(crate::DEFAULT_MAX_STACK_DEPTH as usize),
        }
    }

    /// Drop all frames but keep the reserved capacity, so the buffer can be
    /// returned to the interpreter's stack pool and reused without reallocating.
    #[inline]
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// Total number of live frames.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// `true` when no frames are live.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Push `frame` onto the stack.
    ///
    /// The frame is fully constructed before it is published (its register
    /// window is already `Value::undefined()`-filled by
    /// [`Frame::with_exec_registers`] and friends), so it is never visible to
    /// GC in a partially-initialized state. Callers that need the pushed frame
    /// read it back with [`last_mut`](Self::last_mut); that reference stays valid
    /// until the matching [`pop`](Self::pop) / [`truncate`](Self::truncate),
    /// because the reserved buffer never reallocates.
    #[inline]
    pub fn push(&mut self, frame: Frame) {
        self.frames.push(frame);
    }

    /// Publish a fully initialized call-frame reservation onto the stack.
    ///
    /// The returned descriptor is valid until the frame is popped/truncated.
    /// Because every `HoltStack` is pre-reserved to the VM stack-depth limit,
    /// publishing the frame cannot reallocate and cannot move older frames.
    #[inline]
    pub fn publish_call_reservation(&mut self, reservation: HoltCallReservation) -> HoltFrameDesc {
        let index = self.frames.len();
        self.frames.push(reservation.frame);
        self.frame_desc(index)
            .expect("published frame descriptor must exist")
    }

    /// Descriptor for a live frame at `index`.
    #[inline]
    #[must_use]
    pub fn frame_desc(&mut self, index: usize) -> Option<HoltFrameDesc> {
        let frame = self.frames.get_mut(index)?;
        Some(HoltFrameDesc {
            index,
            value_slots: HoltValueSlots {
                ptr: frame.registers.as_mut_ptr(),
                len: frame.registers.len(),
            },
        })
    }

    /// Pop and return the top frame, or `None` when empty.
    #[inline]
    pub fn pop(&mut self) -> Option<Frame> {
        self.frames.pop()
    }

    /// Shared reference to the top frame.
    #[inline]
    #[must_use]
    pub fn last(&self) -> Option<&Frame> {
        self.frames.last()
    }

    /// Mutable reference to the top frame.
    #[inline]
    #[must_use]
    pub fn last_mut(&mut self) -> Option<&mut Frame> {
        self.frames.last_mut()
    }

    /// Shared reference to the top frame without a bounds check.
    ///
    /// # Safety
    /// The stack must be non-empty. The dispatch loop calls this only after its
    /// `is_empty()` guard at the top of each tick, so a live top frame is
    /// guaranteed; the reserved buffer never reallocates, so the reference stays
    /// put until the next push/pop. Avoids the `Index`/`last` bounds check on the
    /// hottest per-instruction read.
    #[inline]
    #[must_use]
    pub unsafe fn top_unchecked(&self) -> &Frame {
        let len = self.frames.len();
        debug_assert!(len > 0, "top_unchecked on empty stack");
        unsafe { self.frames.get_unchecked(len - 1) }
    }

    /// Mutable counterpart to [`Self::top_unchecked`].
    ///
    /// # Safety
    /// Same contract: the stack must be non-empty.
    #[inline]
    #[must_use]
    pub unsafe fn top_unchecked_mut(&mut self) -> &mut Frame {
        let len = self.frames.len();
        debug_assert!(len > 0, "top_unchecked_mut on empty stack");
        unsafe { self.frames.get_unchecked_mut(len - 1) }
    }

    /// Shared reference to the frame at index `i` (`0` is the bottom `<main>`
    /// frame), or `None` if out of range.
    #[inline]
    #[must_use]
    pub fn get(&self, i: usize) -> Option<&Frame> {
        self.frames.get(i)
    }

    /// Mutable reference to the frame at index `i`, or `None`.
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self, i: usize) -> Option<&mut Frame> {
        self.frames.get_mut(i)
    }

    /// Drop frames until exactly `new_len` remain. No-op if already shorter.
    /// Each removed [`Frame`] is dropped, matching `Vec::truncate`.
    #[inline]
    pub fn truncate(&mut self, new_len: usize) {
        self.frames.truncate(new_len);
    }

    /// Iterate live frames bottom-to-top (push order). Double-ended so callers
    /// (e.g. backtrace snapshotting) can walk it innermost-first with `.rev()`.
    #[inline]
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Frame> {
        self.frames.iter()
    }

    /// Mutably iterate live frames bottom-to-top.
    #[inline]
    pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut Frame> {
        self.frames.iter_mut()
    }
}

impl std::ops::Index<usize> for HoltStack {
    type Output = Frame;

    #[inline]
    fn index(&self, i: usize) -> &Frame {
        &self.frames[i]
    }
}

impl std::ops::IndexMut<usize> for HoltStack {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut Frame {
        &mut self.frames[i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;
    use otter_bytecode::{
        BytecodeModule, Function, Instruction, SourceKind as BcSourceKind, SpanEntry,
    };

    /// Minimal single-function module whose `<main>` has one scratch register,
    /// so `Frame::for_function` yields a 1-register frame we can stamp with an
    /// identity tag.
    fn one_register_module() -> BytecodeModule {
        BytecodeModule {
            module: "holt-stack-test.ts".to_string(),
            template_sites: Vec::new(),
            source_kind: BcSourceKind::TypeScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch: 1,
                param_count: 0,
                length: 0,
                own_upvalue_count: 0,
                is_strict: false,
                is_arrow: false,
                is_method: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_derived_constructor: false,
                is_module: false,
                needs_arguments: false,
                uses_arguments_callee: false,
                arguments_object_kind: crate::ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                source_text: None,
                source_text_span: None,
                module_url: String::new(),
                direct_eval_bindings: Vec::new(),
                contains_direct_eval: false,
                code: Vec::<Instruction>::new().into(),
                spans: Vec::<SpanEntry>::new(),
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        }
    }

    /// A frame whose single register holds `tag`.
    fn tagged_frame(function: &Function, tag: i32) -> Frame {
        let mut frame = Frame::for_function(function);
        frame.registers[0] = Value::number_i32(tag);
        frame
    }

    #[test]
    fn push_pop_len_and_order() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();
        assert!(stack.is_empty());

        for tag in 0..5 {
            stack.push(tagged_frame(f, tag));
        }
        assert_eq!(stack.len(), 5);

        for (i, frame) in stack.iter().enumerate() {
            assert_eq!(frame.registers[0], Value::number_i32(i as i32));
            assert_eq!(stack[i].registers[0], Value::number_i32(i as i32));
        }

        assert_eq!(stack.last().unwrap().registers[0], Value::number_i32(4));
        let popped = stack.pop().unwrap();
        assert_eq!(popped.registers[0], Value::number_i32(4));
        assert_eq!(stack.len(), 4);
    }

    #[test]
    fn dispatch_capacity_keeps_frame_addresses_stable() {
        let module = one_register_module();
        let f = &module.functions[0];
        // A dispatch stack reserves the max call depth, so pushing up to that
        // bound never reallocates and never moves a previously-pushed frame.
        let mut stack = HoltStack::new();
        let n = crate::DEFAULT_MAX_STACK_DEPTH as usize;

        let mut addrs = Vec::with_capacity(n);
        for tag in 0..n {
            stack.push(tagged_frame(f, tag as i32));
            addrs.push(stack.last().unwrap() as *const Frame as usize);
        }
        for (i, &addr) in addrs.iter().enumerate() {
            let now = &stack[i] as *const Frame as usize;
            assert_eq!(now, addr, "frame {i} moved within the reserved capacity");
            assert_eq!(stack[i].registers[0], Value::number_i32(i as i32));
        }
    }

    #[test]
    fn call_reservation_is_unpublished_until_publish() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();
        let mut reservation = HoltCallReservation::from_frame(tagged_frame(f, 7));

        assert_eq!(stack.len(), 0);
        assert_eq!(reservation.value_slots().len(), 1);
        reservation.frame_mut().registers[0] = Value::number_i32(11);

        let desc = reservation.publish(&mut stack);
        assert_eq!(desc.index(), 0);
        assert_eq!(desc.value_slots().len(), 1);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack[0].registers[0], Value::number_i32(11));
    }

    #[test]
    fn published_frame_desc_matches_register_window() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();

        let desc = HoltCallReservation::from_frame(tagged_frame(f, 3)).publish(&mut stack);
        let frame = stack.get_mut(desc.index()).unwrap();

        assert_eq!(
            desc.value_slots().as_mut_ptr(),
            frame.registers.as_mut_ptr()
        );
        assert_eq!(desc.value_slots().len(), frame.registers.len());
    }

    #[test]
    fn truncate_drops_to_target() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();
        for tag in 0..100 {
            stack.push(tagged_frame(f, tag));
        }

        stack.truncate(40);
        assert_eq!(stack.len(), 40);
        assert_eq!(stack.last().unwrap().registers[0], Value::number_i32(39));

        // Truncate longer-than-len is a no-op.
        stack.truncate(10_000);
        assert_eq!(stack.len(), 40);

        stack.truncate(0);
        assert!(stack.is_empty());
    }

    #[test]
    fn get_and_get_mut_bounds() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();
        for tag in 0..3 {
            stack.push(tagged_frame(f, tag));
        }
        assert!(stack.get(3).is_none());
        assert!(stack.get_mut(3).is_none());
        stack.get_mut(1).unwrap().registers[0] = Value::number_i32(99);
        assert_eq!(stack[1].registers[0], Value::number_i32(99));
    }

    #[test]
    fn iter_traces_every_live_frame() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();
        let n = 200;
        for tag in 0..n as i32 {
            stack.push(tagged_frame(f, tag));
        }

        // Mirrors how `trace_active_frame_roots` walks the live stack for GC:
        // iterate every frame and trace its slots. number_i32 registers carry
        // no GC pointer, so a correct walk visits zero raw-GC slots and reaches
        // every live frame without panicking. The closure is scoped so its
        // borrow of `visited_slots` releases before the assert.
        let mut visited_slots = 0usize;
        {
            let mut visitor = |_p: *mut otter_gc::raw::RawGc| {
                visited_slots += 1;
            };
            for frame in stack.iter() {
                frame.trace_frame_slots(&mut visitor);
            }
        }
        assert_eq!(visited_slots, 0);
        assert_eq!(stack.len(), n);
    }
}
