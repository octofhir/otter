//! Materialized interpreter activation stack.
//!
//! The stack owns bytecode-interpreter frames while register storage lives in
//! the separate reservation-stable [`crate::RegisterStack`]. The frame vector
//! may therefore grow and move: consumers keep indices and register/upvalue
//! windows, never `Frame` addresses, across a push.
//!
//! # Contents
//! - [`HoltStack`] — the frame stack and its stack-discipline API.
//!
//! # Invariants
//! - No reference or pointer to a `Frame` survives an operation that can push.
//! - Register windows remain stable independently of frame-vector growth.
//! - GC tracing visits every live frame exactly once, in push order.
//!
//! # See also
//! - [`crate::frame_state::Frame`] — the frame payload and `trace_frame_slots`.
//! - [`crate::cold_frame`] — cold side records traced separately from hot frame
//!   state.

use crate::frame_state::Frame;

/// Growable stack of materialized interpreter call [`Frame`]s.
///
/// Stack-discipline surface (`push` / `pop` / `last` /
/// `last_mut` / `len` / `is_empty` / `get` / `get_mut` / `truncate` / `clear` /
/// `iter` / `iter_mut`) plus `Index` / `IndexMut`.
#[derive(Debug)]
pub struct HoltStack {
    frames: Vec<Frame>,
}

impl Default for HoltStack {
    fn default() -> Self {
        Self::new()
    }
}

impl HoltStack {
    /// A new empty stack. Capacity grows only with observed call depth.
    #[must_use]
    pub fn new() -> Self {
        Self { frames: Vec::new() }
    }

    /// Drop all frames but retain observed capacity for pooled reuse.
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
    /// [`Frame::with_exec_return_upvalues_and_this`] and friends), so it is never visible to
    /// GC in a partially-initialized state.
    #[inline]
    pub fn push(&mut self, frame: Frame) {
        self.frames.push(frame);
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
    /// guaranteed. The reference must not survive a push/pop. Avoids the
    /// `Index`/`last` bounds check on the hottest per-instruction read.
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
    fn tagged_frame(
        function: &Function,
        tag: i32,
        registers: &mut crate::register_stack::RegisterStack,
    ) -> Frame {
        let window = registers.allocate(1).unwrap();
        let mut frame = Frame::for_function(function, window);
        frame.registers[0] = Value::number_i32(tag);
        frame
    }

    #[test]
    fn push_pop_len_and_order() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();
        let mut registers = crate::register_stack::RegisterStack::new();
        assert!(stack.is_empty());

        for tag in 0..5 {
            stack.push(tagged_frame(f, tag, &mut registers));
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
    fn capacity_grows_with_observed_depth_without_moving_register_windows() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();
        let mut registers = crate::register_stack::RegisterStack::new();
        assert_eq!(stack.frames.capacity(), 0);
        let n = 32;

        for tag in 0..n {
            stack.push(tagged_frame(f, tag as i32, &mut registers));
        }
        assert!(stack.frames.capacity() >= n);
        assert!(stack.frames.capacity() < crate::DEFAULT_MAX_STACK_DEPTH as usize);
        for i in 0..n {
            assert_eq!(stack[i].registers[0], Value::number_i32(i as i32));
        }
    }

    #[test]
    fn truncate_drops_to_target() {
        let module = one_register_module();
        let f = &module.functions[0];
        let mut stack = HoltStack::new();
        let mut registers = crate::register_stack::RegisterStack::new();
        for tag in 0..100 {
            stack.push(tagged_frame(f, tag, &mut registers));
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
        let mut registers = crate::register_stack::RegisterStack::new();
        for tag in 0..3 {
            stack.push(tagged_frame(f, tag, &mut registers));
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
        let mut registers = crate::register_stack::RegisterStack::new();
        let n = 200;
        for tag in 0..n as i32 {
            stack.push(tagged_frame(f, tag, &mut registers));
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
