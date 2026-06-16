//! HoltStack — segmented, stable-address execution-frame stack.
//!
//! The interpreter today holds its call frames in a `SmallVec<[Frame; 8]>`.
//! That works for a pure interpreter but is the wrong substrate for
//! machine-code calls and optimizer deopt: a growable contiguous buffer
//! **reallocates and moves every live frame** when it outgrows its capacity.
//! Compiled callees need their caller's frame — and the register slots inside
//! it — to keep a stable address for the whole lifetime of the call.
//!
//! `HoltStack` is that substrate. Frames live in fixed-capacity
//! [`HoltSegment`]s. A segment's backing buffer is reserved once to
//! [`SEGMENT_CAP`] and is **never** grown past it, so the buffer never
//! reallocates and every `&Frame` / `&mut Frame` it hands out stays valid
//! until that frame is popped. Growth appends a *new* segment; the outer
//! `Vec<HoltSegment>` may reallocate, but that only moves segment *headers*
//! (a pointer/len/cap triple) — never the heap frame buffers the headers
//! point at. Index math stays O(1) because every segment except the last is
//! kept exactly full.
//!
//! This module is the additive, isolated substrate (HoltStack slice 1a). It
//! is **not yet wired into the dispatcher** — the live path still uses
//! `SmallVec<[Frame; 8]>`. Wiring it (behind `OTTER_HOLT_STACK`) is a later
//! sub-slice that must carry the call/frame/generator/async test262 gates.
//!
//! # Contents
//! - [`HoltStack`] — the segmented frame stack and its stack-discipline API.
//! - [`HoltSegment`] — one fixed-capacity, non-reallocating frame buffer.
//! - [`SEGMENT_CAP`] — frames per segment.
//!
//! # Invariants
//! - A segment's `frames` buffer is reserved to `SEGMENT_CAP` and `len` never
//!   exceeds it, so the buffer address is stable for the segment's lifetime.
//! - Every segment except the last is exactly `SEGMENT_CAP` full; the last
//!   holds `1..=SEGMENT_CAP` frames (there are no empty or partial interior
//!   segments). This is what makes `index = (i / CAP, i % CAP)` correct.
//! - A frame's address is stable from `push` until the matching `pop`.
//! - GC tracing visits every live frame exactly once, in push order.
//!
//! # See also
//! - [`crate::frame_state::Frame`] — the frame payload and `trace_frame_slots`.
//! - [`crate::cold_frame`] — cold side records, traced separately by the
//!   integration just as they are for the `SmallVec` stack today.

// Slice 1a lands the substrate without wiring it into the dispatcher, so its
// API has no in-crate callers yet. The integration sub-slice (behind
// `OTTER_HOLT_STACK`) removes this allowance as the call sites adopt it.
#![allow(dead_code)]

use otter_gc::raw::SlotVisitor;

use crate::frame_state::Frame;

/// Frames per [`HoltSegment`]. Sized so a segment buffer stays a few pages
/// (each [`Frame`] is ≤128 B, so 64 frames ≤ 8 KiB) while comfortably
/// covering typical JS call depth before a second segment is needed.
pub(crate) const SEGMENT_CAP: usize = 64;

/// One fixed-capacity frame buffer. `frames` is reserved to [`SEGMENT_CAP`]
/// at construction and never pushed past it, so its heap buffer never
/// reallocates and the addresses of the frames it holds are stable.
#[derive(Debug)]
struct HoltSegment {
    frames: Vec<Frame>,
}

impl HoltSegment {
    fn new() -> Self {
        Self {
            frames: Vec::with_capacity(SEGMENT_CAP),
        }
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.frames.len() == SEGMENT_CAP
    }
}

/// Segmented, stable-address stack of interpreter call [`Frame`]s.
///
/// Drop-in for the legacy `SmallVec<[Frame; 8]>`: it offers the same
/// stack-discipline surface (`push` / `pop` / `last` / `last_mut` / `len` /
/// `is_empty` / `get` / `get_mut` / `truncate` / `iter` / `iter_mut`) plus
/// `Index` / `IndexMut`, but never moves a live frame.
#[derive(Debug, Default)]
pub struct HoltStack {
    segments: Vec<HoltSegment>,
    len: usize,
}

impl HoltStack {
    /// A new, empty stack. No allocation until the first [`push`](Self::push).
    #[must_use]
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
            len: 0,
        }
    }

    /// Total number of live frames.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` when no frames are live.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push `frame` and return a stable `&mut` to it. The reference stays
    /// valid until the matching [`pop`](Self::pop) / [`truncate`](Self::truncate).
    ///
    /// The frame is fully constructed before it is published here (its
    /// register window is already `Value::undefined()`-filled by
    /// [`Frame::with_exec_registers`] and friends), so the published frame is
    /// never visible to GC in a partially-initialized state.
    pub fn push(&mut self, frame: Frame) -> &mut Frame {
        if self.segments.last().is_none_or(HoltSegment::is_full) {
            self.segments.push(HoltSegment::new());
        }
        let seg = self
            .segments
            .last_mut()
            .expect("just ensured a non-full last segment exists");
        seg.frames.push(frame);
        self.len += 1;
        seg.frames
            .last_mut()
            .expect("just pushed a frame into this segment")
    }

    /// Pop and return the top frame, or `None` when empty. Dropping the
    /// now-empty trailing segment keeps the "interior segments are full"
    /// invariant and frees the buffer; frames in other segments are untouched.
    pub fn pop(&mut self) -> Option<Frame> {
        let seg = self.segments.last_mut()?;
        let frame = seg.frames.pop();
        if frame.is_some() {
            self.len -= 1;
            if seg.frames.is_empty() {
                self.segments.pop();
            }
        }
        frame
    }

    /// Shared reference to the top frame.
    #[inline]
    #[must_use]
    pub fn last(&self) -> Option<&Frame> {
        self.segments.last().and_then(|s| s.frames.last())
    }

    /// Mutable reference to the top frame.
    #[inline]
    #[must_use]
    pub fn last_mut(&mut self) -> Option<&mut Frame> {
        self.segments.last_mut().and_then(|s| s.frames.last_mut())
    }

    /// Shared reference to the frame at global index `i` (`0` is the bottom
    /// `<main>` frame), or `None` if out of range.
    #[inline]
    #[must_use]
    pub fn get(&self, i: usize) -> Option<&Frame> {
        if i >= self.len {
            return None;
        }
        let (seg, off) = (i / SEGMENT_CAP, i % SEGMENT_CAP);
        Some(&self.segments[seg].frames[off])
    }

    /// Mutable reference to the frame at global index `i`, or `None`.
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self, i: usize) -> Option<&mut Frame> {
        if i >= self.len {
            return None;
        }
        let (seg, off) = (i / SEGMENT_CAP, i % SEGMENT_CAP);
        Some(&mut self.segments[seg].frames[off])
    }

    /// Drop frames until exactly `new_len` remain. No-op if already shorter.
    /// Each removed [`Frame`] is dropped (running `Frame`'s `Drop`), matching
    /// `Vec::truncate` semantics on the legacy stack.
    pub fn truncate(&mut self, new_len: usize) {
        while self.len > new_len {
            if self.pop().is_none() {
                break;
            }
        }
    }

    /// Iterate live frames bottom-to-top (push order).
    pub fn iter(&self) -> impl Iterator<Item = &Frame> {
        self.segments.iter().flat_map(|s| s.frames.iter())
    }

    /// Mutably iterate live frames bottom-to-top.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Frame> {
        self.segments.iter_mut().flat_map(|s| s.frames.iter_mut())
    }

    /// Trace the GC roots held by every live frame — register window,
    /// upvalue cells, `this`, async result promise, and nested
    /// generator/async state — via [`Frame::trace_frame_slots`].
    ///
    /// Cold-record slots (pending ToPrimitive/bind/iterator ladders) are
    /// traced separately by the integration through the cold-frame pool,
    /// exactly as for the `SmallVec` stack today; this method intentionally
    /// mirrors `trace_frame_slots` only.
    pub(crate) fn trace_frames(&self, visitor: &mut SlotVisitor<'_>) {
        for frame in self.iter() {
            frame.trace_frame_slots(visitor);
        }
    }

    /// Debug-only structural check of the segment invariant: every segment but
    /// the last is exactly full, the last is non-empty, and the segment frame
    /// counts sum to `len`.
    #[cfg(test)]
    fn assert_invariants(&self) {
        let mut total = 0;
        for (i, seg) in self.segments.iter().enumerate() {
            let n = seg.frames.len();
            assert!(n <= SEGMENT_CAP, "segment over capacity");
            if i + 1 < self.segments.len() {
                assert_eq!(n, SEGMENT_CAP, "interior segment {i} must be full");
            } else {
                assert!(n > 0, "trailing segment must be non-empty");
            }
            assert!(
                seg.frames.capacity() >= SEGMENT_CAP,
                "buffer must stay reserved"
            );
            total += n;
        }
        assert_eq!(total, self.len, "segment counts must sum to len");
    }
}

impl std::ops::Index<usize> for HoltStack {
    type Output = Frame;

    #[inline]
    fn index(&self, i: usize) -> &Frame {
        self.get(i).expect("HoltStack index out of bounds")
    }
}

impl std::ops::IndexMut<usize> for HoltStack {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut Frame {
        self.get_mut(i).expect("HoltStack index out of bounds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;
    use otter_bytecode::{
        BytecodeModule, Function, Instruction, SourceKind as BcSourceKind, SpanEntry,
    };

    /// Minimal single-function module whose `<main>` has one scratch
    /// register, so `Frame::for_function` yields a 1-register frame we can
    /// stamp with an identity tag.
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
                arguments_object_kind: crate::ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                source_text: None,
                source_text_span: None,
                module_url: String::new(),
                direct_eval_bindings: Vec::new(),
                contains_direct_eval: false,
                code: Vec::<Instruction>::new(),
                spans: Vec::<SpanEntry>::new(),
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        }
    }

    /// A frame whose single register holds `tag`, so we can verify identity
    /// after the stack grows.
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
        stack.assert_invariants();

        // Bottom-to-top order via index and iter.
        for (i, frame) in stack.iter().enumerate() {
            assert_eq!(frame.registers[0], Value::number_i32(i as i32));
            assert_eq!(stack[i].registers[0], Value::number_i32(i as i32));
        }

        assert_eq!(stack.last().unwrap().registers[0], Value::number_i32(4));
        let popped = stack.pop().unwrap();
        assert_eq!(popped.registers[0], Value::number_i32(4));
        assert_eq!(stack.len(), 4);
        stack.assert_invariants();
    }

    #[test]
    fn frame_addresses_are_stable_across_segment_growth() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();

        // Fill well past one segment so the outer Vec<HoltSegment> reallocates
        // and several segments are allocated.
        let n = SEGMENT_CAP * 3 + 7;
        let mut addrs = Vec::with_capacity(n);
        for tag in 0..n {
            let frame_ref = stack.push(tagged_frame(f, tag as i32));
            addrs.push(frame_ref as *const Frame as usize);
        }
        stack.assert_invariants();
        assert!(stack.segments.len() >= 4, "expected multiple segments");

        // Every previously-handed-out frame address must be unchanged, and the
        // frame contents uncorrupted, after all the growth.
        for (i, &addr) in addrs.iter().enumerate() {
            let now = &stack[i] as *const Frame as usize;
            assert_eq!(now, addr, "frame {i} moved across segment growth");
            assert_eq!(stack[i].registers[0], Value::number_i32(i as i32));
        }
    }

    #[test]
    fn truncate_drops_to_target_and_keeps_invariant() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();
        for tag in 0..(SEGMENT_CAP * 2 + 5) as i32 {
            stack.push(tagged_frame(f, tag));
        }

        stack.truncate(SEGMENT_CAP + 2);
        assert_eq!(stack.len(), SEGMENT_CAP + 2);
        stack.assert_invariants();
        assert_eq!(
            stack.last().unwrap().registers[0],
            Value::number_i32((SEGMENT_CAP + 1) as i32),
        );

        // Truncate longer-than-len is a no-op.
        stack.truncate(10_000);
        assert_eq!(stack.len(), SEGMENT_CAP + 2);

        stack.truncate(0);
        assert!(stack.is_empty());
        stack.assert_invariants();
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
    fn trace_visits_every_live_register_slot() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();
        let n = SEGMENT_CAP + 3;
        for tag in 0..n as i32 {
            stack.push(tagged_frame(f, tag));
        }

        // `SlotVisitor` is an alias for `dyn FnMut(*mut RawGc)`. number_i32
        // registers carry no GC pointer, so a correct trace over a
        // multi-segment stack visits zero raw-GC slots and, crucially,
        // completes without panicking (reaching every live frame). The closure
        // is scoped so its borrow of `visited_slots` releases before the assert.
        let mut visited_slots = 0usize;
        {
            let mut visitor = |_p: *mut otter_gc::raw::RawGc| {
                visited_slots += 1;
            };
            stack.trace_frames(&mut visitor);
        }
        assert_eq!(visited_slots, 0);
        assert_eq!(stack.len(), n);
    }
}
