//! Non-owning view of the common native JavaScript frame chain.
//!
//! # Contents
//! - [`ActivationStack`] indexes caller-linked frames published by execution.
//! - [`ActivationFloor`] bounds one execution region.
//! - Queued call ownership and runtime-turn root publication.
//!
//! # Invariants
//! The trampoline owns active frames and registers. This view owns only a
//! prepared input packet and borrows the execution context's frame cell.
//! Completing a frame changes its logical visibility while its physical roots
//! remain published until the trampoline returns. Host frames keep their
//! caller's depth and are skipped: they are physical, not logical, activations. Iteration never changes
//! linkage. Queued input and cold records stay rooted before native entry.
//!
//! # See also
//! - [`crate::native_abi::call_trampoline`] for stack storage and dispatch.
//! - [`crate::prepared_call`] for inputs before publication.

use crate::{
    Interpreter, PreparedCall,
    native_abi::{Frame, JitCtx},
    runtime_cx::RuntimeTurn,
};
use otter_gc::raw::RawGc;

/// First frame at or below `frame` that is a JavaScript activation. Host
/// frames carry their caller's depth and are not logical activations.
fn skip_host_frames(mut frame: *mut Frame) -> *mut Frame {
    // SAFETY: published frames link only to live caller records.
    while !frame.is_null()
        && unsafe { (*frame).header.kind } == crate::native_abi::NativeFrameKind::Host
    {
        frame = unsafe { (*frame).caller_frame() };
    }
    frame
}

/// View of caller-linked JavaScript activations for one mutator turn.
#[derive(Debug)]
pub struct ActivationStack {
    context: *mut JitCtx,
    pending: Option<PreparedCall>,
    /// Classify request awaiting the trampoline; its span is `staged`.
    request: Option<crate::native_abi::CallRequest>,
    /// Actual arguments of `request`, copied by the trampoline before any
    /// allocation. Cleared by the next execution turn.
    staged: smallvec::SmallVec<[crate::Value; 8]>,
    runtime_root_owner: Option<usize>,
}

struct RuntimeRootedStack<'a> {
    stack: &'a mut ActivationStack,
}
impl RuntimeRootedStack<'_> {
    fn as_ptr(&self) -> *const ActivationStack {
        self.stack
    }
    fn as_mut(&mut self) -> &mut ActivationStack {
        self.stack
    }
}
impl Drop for RuntimeRootedStack<'_> {
    fn drop(&mut self) {
        self.stack.runtime_root_owner = None;
    }
}

/// Number of caller frames below an execution region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivationFloor {
    depth: usize,
}
impl ActivationFloor {
    /// Empty caller region.
    pub const ROOT: Self = Self { depth: 0 };
    /// Number of caller-owned frames.
    pub const fn depth(self) -> usize {
        self.depth
    }
    pub(crate) const fn at_depth(depth: usize) -> Self {
        Self { depth }
    }
}
impl Default for ActivationStack {
    fn default() -> Self {
        Self::new()
    }
}
impl ActivationStack {
    /// Empty activation view; native execution binds its frame cell.
    pub fn new() -> Self {
        Self {
            context: std::ptr::null_mut(),
            pending: None,
            request: None,
            staged: smallvec::SmallVec::new(),
            runtime_root_owner: None,
        }
    }
    fn enter_runtime_rooted(&mut self, owner: usize) -> RuntimeRootedStack<'_> {
        assert!(self.runtime_root_owner.is_none());
        self.runtime_root_owner = Some(owner);
        RuntimeRootedStack { stack: self }
    }
    pub(crate) fn is_runtime_rooted_by(&self, interp: &Interpreter) -> bool {
        self.runtime_root_owner == Some(interp as *const Interpreter as usize)
    }
    pub(crate) unsafe fn bind_context(&mut self, context: *mut JitCtx) -> *mut JitCtx {
        std::mem::replace(&mut self.context, context)
    }
    pub(crate) fn restore_context(&mut self, previous: *mut JitCtx) {
        self.context = previous;
    }
    pub(crate) fn clear_completion(&mut self) {
        if self.context.is_null() {
            return;
        }
        let current = unsafe { (*self.context).native_frame };
        if let Some(frame) = unsafe { current.as_mut() } {
            let completed = crate::native_abi::NativeFrameFlags::COMPLETED;
            let bits = frame.header.flags.bits() & !completed;
            frame.header.flags = crate::native_abi::NativeFrameFlags::from_bits(bits);
        }
    }
    pub(crate) fn execution_context(&self) -> *mut JitCtx {
        self.context
    }
    pub(crate) fn pending_mut(&mut self) -> Option<&mut PreparedCall> {
        self.pending.as_mut()
    }

    pub(crate) fn has_pending_call(&self) -> bool {
        self.pending.is_some() || self.request.is_some()
    }

    /// Whether owned inputs for a VM-selected entry are queued.
    pub(crate) fn has_prepared_call(&self) -> bool {
        self.pending.is_some()
    }

    /// Queue an ordinary `[[Call]]` (or `[[Construct]]` when `new_target` is
    /// present) for the trampoline to classify. Nothing about the callee is
    /// inspected here.
    pub(crate) fn stage_call(
        &mut self,
        callee: crate::Value,
        receiver: crate::Value,
        new_target: Option<crate::Value>,
        arguments: impl IntoIterator<Item = crate::Value>,
        return_register: Option<u16>,
    ) {
        assert!(
            !self.has_pending_call(),
            "one pending call per execution boundary"
        );
        self.staged.clear();
        self.staged.extend(arguments);
        let mut request = crate::native_abi::CallRequest::EMPTY;
        request.callee = callee;
        request.receiver = receiver;
        if let Some(new_target) = new_target {
            request.new_target = new_target;
            request.header.flags = crate::native_abi::NativeFrameFlags::from_bits(
                crate::native_abi::NativeFrameFlags::CONSTRUCT,
            );
        }
        request.return_destination = return_register.map_or(u32::MAX, u32::from);
        self.request = Some(request);
    }

    /// Stage only the actual span of a request a generated caller writes
    /// itself. Returns the span; it stays valid until the next staging.
    pub(crate) fn stage_arguments(
        &mut self,
        arguments: impl IntoIterator<Item = crate::Value>,
    ) -> Option<(*const crate::Value, u32)> {
        assert!(
            self.request.is_none(),
            "one pending call per execution boundary"
        );
        self.staged.clear();
        self.staged.extend(arguments);
        Some((self.staged.as_ptr(), u32::try_from(self.staged.len()).ok()?))
    }

    /// The staged classify request, if any.
    pub(crate) fn staged_request_mut(&mut self) -> Option<&mut crate::native_abi::CallRequest> {
        self.request.as_mut()
    }

    /// Hand the staged request to the trampoline. Its span stays valid until
    /// the next staging or [`Self::clear_staged`].
    pub(crate) fn take_request(&mut self) -> Option<crate::native_abi::CallRequest> {
        let mut request = self.request.take()?;
        request.arguments = self.staged.as_ptr();
        request.argument_count = u32::try_from(self.staged.len()).ok()?;
        Some(request)
    }

    /// Drop a consumed request span so it retains no values.
    pub(crate) fn clear_staged(&mut self) {
        if self.request.is_none() {
            self.staged.clear();
        }
    }
    pub(crate) fn pending_packet(&self) -> Option<crate::native_abi::CallRequest> {
        self.pending.as_ref().map(PreparedCall::packet)
    }
    pub(crate) fn consume_pending(&mut self) -> crate::prepared_call::ResumeInput {
        if let Some(mut call) = self.pending.take() {
            self.pending = call.child.take().map(|child| *child);
            call.resume
        } else {
            crate::prepared_call::ResumeInput::Normal
        }
    }
    pub(crate) fn take_pending(&mut self) -> Option<PreparedCall> {
        self.pending.take()
    }
    pub(crate) fn push(&mut self, call: PreparedCall) {
        assert!(
            self.pending.is_none(),
            "one pending call per execution boundary"
        );
        self.pending = Some(call);
    }
    fn top_pointer(&self) -> *mut Frame {
        if self.context.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: the execution extent binds a live context and frame chain.
        let mut current = unsafe { (*self.context).native_frame };
        if !current.is_null()
            && unsafe {
                (*current)
                    .header
                    .flags
                    .contains(crate::native_abi::NativeFrameFlags::COMPLETED)
            }
        {
            current = unsafe { (*current).caller_frame() };
        }
        skip_host_frames(current)
    }
    fn frame_pointer(&self, index: usize) -> *mut Frame {
        let mut frame = self.top_pointer();
        while !frame.is_null() {
            let depth = unsafe { (*frame).depth } as usize;
            if depth == index + 1 {
                return frame;
            }
            if depth <= index {
                break;
            }
            frame = skip_host_frames(unsafe { (*frame).caller_frame() });
        }
        std::ptr::null_mut()
    }
    /// Total live logical depth in the published chain.
    pub fn len(&self) -> usize {
        unsafe { self.top_pointer().as_ref() }.map_or(0, |f| f.depth as usize)
    }
    /// Whether this view has no live logical activation.
    pub fn is_empty(&self) -> bool {
        self.top_pointer().is_null()
    }
    /// Mark the current activation as logically complete. Physical ownership
    /// and root publication remain with the trampoline until its entry returns.
    pub(crate) fn pop(&mut self) -> Option<&mut Frame> {
        let frame = self.top_pointer();
        if frame.is_null() {
            return None;
        }
        assert_eq!(
            frame,
            unsafe { (*self.context).native_frame },
            "an entry completes only its own activation"
        );
        unsafe {
            (*frame).header.flags = crate::native_abi::NativeFrameFlags::from_bits(
                (*frame).header.flags.bits() | crate::native_abi::NativeFrameFlags::COMPLETED,
            );
        }
        unsafe { frame.as_mut() }
    }
    /// Innermost live logical frame.
    pub fn last(&self) -> Option<&Frame> {
        unsafe { self.top_pointer().as_ref() }
    }
    /// Mutable innermost live logical frame.
    pub fn last_mut(&mut self) -> Option<&mut Frame> {
        unsafe { self.top_pointer().as_mut() }
    }
    /// Frame at an absolute chain index.
    pub fn get(&self, index: usize) -> Option<&Frame> {
        unsafe { self.frame_pointer(index).as_ref() }
    }
    /// Mutable frame at an absolute chain index.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut Frame> {
        unsafe { self.frame_pointer(index).as_mut() }
    }
    /// Innermost frame without a null check.
    /// # Safety
    /// The logical chain must be non-empty.
    pub unsafe fn top_unchecked(&self) -> &Frame {
        unsafe { &*self.top_pointer() }
    }
    /// Mutable innermost frame without a null check.
    /// # Safety
    /// The logical chain must be non-empty and exclusively mutator-owned.
    pub unsafe fn top_unchecked_mut(&mut self) -> &mut Frame {
        unsafe { &mut *self.top_pointer() }
    }
    /// Current caller depth marker.
    pub fn floor(&self) -> ActivationFloor {
        ActivationFloor::at_depth(self.len())
    }
    /// Frames above a caller marker.
    pub fn len_above(&self, floor: ActivationFloor) -> usize {
        self.len().saturating_sub(floor.depth)
    }
    /// Whether no logical frame remains above a caller marker.
    pub fn is_at_floor(&self, floor: ActivationFloor) -> bool {
        self.len() <= floor.depth
    }
    /// Iterate the native chain from outermost to innermost.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Frame> {
        (0..self.len()).map(move |index| self.get(index).expect("complete caller chain"))
    }
    /// Mutably iterate distinct native frames from outermost to innermost.
    pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut Frame> {
        let len = self.len();
        let stack = std::ptr::from_mut(self);
        (0..len).map(move |index| unsafe { &mut *(*stack).frame_pointer(index) })
    }
    pub(crate) fn trace_roots(
        &self,
        cold: &crate::cold_frame::ColdFramePool,
        visitor: &mut dyn FnMut(*mut RawGc),
    ) {
        if let Some(call) = &self.pending {
            call.trace_slots(visitor);
        }
        if let Some(request) = &self.request {
            for value in [&request.callee, &request.receiver, &request.new_target] {
                value.trace_value_slots(visitor);
            }
        }
        for value in &self.staged {
            value.trace_value_slots(visitor);
        }
        cold.trace_all(visitor);
    }
}

impl Interpreter {
    /// Run `body` inside one explicit mutator turn rooted by `stack`.
    ///
    /// The exact stack is marked before its `RawFrameRoots` provider becomes
    /// visible and unmarked only after both collector registrations leave
    /// scope. The private marker makes a [`RuntimeTurn`] impossible to create
    /// for an unrelated or unregistered activation stack.
    pub(crate) fn with_runtime_turn<R>(
        &mut self,
        stack: &mut ActivationStack,
        body: impl FnOnce(RuntimeTurn<'_>) -> R,
    ) -> R {
        let enclosing_cell = self.jit_frame_cell;
        if let Some(context) = unsafe { stack.execution_context().as_mut() } {
            if context.native_frame.is_null() {
                context.native_frame = self.jit_innermost_native_frame();
            }
            self.jit_frame_cell = Some(std::ptr::NonNull::from(&mut context.native_frame).cast());
        }
        let owner = self as *const Interpreter as usize;
        let cold_frames = &self.cold_frames as *const crate::cold_frame::ColdFramePool;
        let mut rooted = stack.enter_runtime_rooted(owner);
        let frame_roots = otter_gc::RawFrameRoots::new(
            rooted.as_ptr(),
            cold_frames,
            ActivationStack::trace_roots,
        );
        let provider: &dyn otter_gc::FrameRoots = &frame_roots;
        let frame_roots_guard = self
            .gc_heap
            .register_frame_roots(provider as *const dyn otter_gc::FrameRoots);
        let extra_roots = otter_gc::ExtraRoots::new(self as &Interpreter);
        let extra_roots_guard = self.gc_heap.register_extra_roots(extra_roots);
        self.runtime_turn_depth += 1;
        let result = body(RuntimeTurn::from_rooted_parts(self, rooted.as_mut()));
        self.runtime_turn_depth -= 1;
        if self.runtime_turn_depth == 0 {
            // No Rust frame of this turn still holds a fresh shape.
            self.shape_runtime.unpin_turn_shapes();
        }
        drop(extra_roots_guard);
        drop(frame_roots_guard);
        self.jit_frame_cell = enclosing_cell;
        result
    }
}

impl std::ops::Index<usize> for ActivationStack {
    type Output = Frame;
    fn index(&self, index: usize) -> &Frame {
        self.get(index).expect("live activation index")
    }
}
impl std::ops::IndexMut<usize> for ActivationStack {
    fn index_mut(&mut self, index: usize) -> &mut Frame {
        self.get_mut(index).expect("live activation index")
    }
}
