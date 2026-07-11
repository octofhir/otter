//! Register-window and frame-stack management.
//!
//! # Contents
//! Register drawing/reclaim on the contiguous reg stack, HoltStack
//! draw/return, cold-frame attach/detach, frame pop/unwind
//! (`pop_frame`, `unwind_abrupt`, `return_running_finally`), and the
//! raw pointers compiled code uses to address the reg window.
//!
//! # Invariants
//! The reg stack is a GC root region: windows must be zeroed on alloc
//! and truncated on reclaim so stale slots never masquerade as values.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    /// Borrow the cold record attached to `frame`, if any.
    #[inline]
    #[must_use]
    pub(crate) fn frame_cold(&self, frame: &Frame) -> Option<&cold_frame::ColdFrame> {
        frame.cold.map(|idx| self.cold_frames.get(idx))
    }

    /// Mutable borrow of the cold record attached to `frame`, if any.
    #[inline]
    #[must_use]
    pub(crate) fn frame_cold_mut(
        &mut self,
        frame: &mut Frame,
    ) -> Option<&mut cold_frame::ColdFrame> {
        frame.cold.map(|idx| self.cold_frames.get_mut(idx))
    }

    /// Acquire a cold record for `frame` if it doesn't have one yet,
    /// Build a `register_count`-wide register window, drawing a spilled
    /// backing buffer from [`Self::reg_pool`] when one is available so a hot
    /// call need not `malloc` a fresh `Vec` per frame. Windows that fit inline
    /// (`<= 8`) never spill and never touch the pool.
    #[inline]
    pub(crate) fn draw_registers(&mut self, total: usize) -> SmallVec<[Value; 8]> {
        if total > 8 {
            while let Some(mut buf) = self.reg_pool.pop() {
                // Defensive: only reuse buffers whose capacity is in a sane
                // band, so one giant frame can't pin an oversized allocation.
                if buf.capacity() <= Self::REG_POOL_MAX_CAP {
                    buf.clear();
                    buf.resize(total, Value::undefined());
                    return SmallVec::from_vec(buf);
                }
            }
        }
        let mut regs: SmallVec<[Value; 8]> = SmallVec::with_capacity(total);
        regs.resize(total, Value::undefined());
        regs
    }

    /// Return a terminated frame's spilled register backing to the pool for
    /// reuse. Inline windows (and a full pool) are dropped normally. The buffer
    /// is cleared, so it carries no live `Value`s — the pool is never traced.
    /// Total slots in the flat JIT register stack. A frameless JIT call chain
    /// exceeding this throws a stack overflow (the same bound the frame-depth
    /// limit enforces, expressed in register slots).
    pub(crate) const REG_STACK_CAP: usize = 512 * 1024;

    /// Base pointer of the flat JIT register stack, allocating its fixed backing
    /// buffer on first use. Stable for the interpreter's life (never
    /// reallocated). Compiled code reads it from `JitCtx.reg_stack_base` to build
    /// self-recursive callee windows inline.
    pub fn jit_reg_stack_base(&mut self) -> *mut u64 {
        if self.reg_stack.capacity() == 0 {
            self.reg_stack = vec![Value::undefined(); Self::REG_STACK_CAP];
        }
        self.reg_stack.as_mut_ptr().cast::<u64>()
    }

    /// Reserve a zero-filled `count`-slot window at the top of the flat register
    /// stack, bumping `reg_top`. Returns the window's base pointer and its slot
    /// index; the caller stores both in a [`FrameRegisters::Window`] and frees
    /// the window by truncating `reg_top` back to the base on frame pop.
    ///
    /// The stack is pre-reserved and never reallocates (live `Window` frames
    /// hold raw pointers into it), so an overflow throws a catchable stack
    /// overflow instead of growing.
    pub(crate) fn alloc_reg_window(&mut self, count: usize) -> Result<(*mut Value, u32), VmError> {
        if self.reg_stack.capacity() == 0 {
            self.reg_stack = vec![Value::undefined(); Self::REG_STACK_CAP];
        }
        let base = self.reg_top;
        let end = base
            .checked_add(count)
            .filter(|&e| e <= Self::REG_STACK_CAP)
            .ok_or(VmError::StackOverflow {
                limit: Self::REG_STACK_CAP as u32,
            })?;
        let window = &mut self.reg_stack[base..end];
        window.fill(Value::undefined());
        let ptr = window.as_mut_ptr();
        self.reg_top = end;
        Ok((ptr, base as u32))
    }

    /// Truncate the flat register stack back to `base`, releasing every window
    /// at or above it. Called when a `Window` frame leaves the stack (return,
    /// unwind, or generator/async park).
    #[inline]
    pub(crate) fn free_reg_window(&mut self, base: u32) {
        // Release this window: truncate the cursor to `base`. Only ever LOWER
        // `reg_top` — never raise it. A window can be freed more than once (a
        // frame returns normally, lowering `reg_top`, then `return_stack` drains
        // the finished re-entry stack and reclaims the same frame); a second
        // free then arrives with `base > reg_top`. Setting `reg_top = base`
        // unconditionally would raise the cursor back over slots that were
        // already released and never re-cleared, so `trace_reg_stack` would then
        // scan those stale register cells as live roots — feeding moved-from /
        // garbage pointers to the scavenger (crashes, wrong dispatch, type
        // mismatches under GC stress). `min` makes a redundant free a no-op.
        self.reg_top = self.reg_top.min(base as usize);
    }

    /// Address of `reg_top` (the live extent of the flat register stack, in
    /// slots). Compiled code reads it from `JitCtx.reg_top_ptr` to reserve and
    /// release callee windows.
    pub fn jit_reg_top_ptr(&mut self) -> *mut usize {
        &mut self.reg_top
    }

    /// Publish the binding scalar slots of a live native JIT context for GC.
    ///
    /// # Safety
    /// Both pointers must name writable boxed-`Value` slots in a native context
    /// that remains live until the matching [`Self::jit_pop_native_activation`].
    pub unsafe fn jit_push_native_activation(
        &mut self,
        self_slot: *mut u64,
        this_slot: *mut u64,
    ) -> Result<(), VmError> {
        if self.jit_native_activation_top >= self.jit_native_activations.len() {
            return Err(VmError::StackOverflow {
                limit: self.max_stack_depth,
            });
        }
        self.jit_native_activations[self.jit_native_activation_top] = jit::JitNativeActivation {
            self_slot,
            this_slot,
        };
        self.jit_native_activation_top += 1;
        Ok(())
    }

    /// Unpublish the most recently pushed native JIT activation.
    #[inline]
    pub fn jit_pop_native_activation(&mut self) {
        debug_assert!(self.jit_native_activation_top > 0);
        self.jit_native_activation_top -= 1;
        self.jit_native_activations[self.jit_native_activation_top] =
            jit::JitNativeActivation::EMPTY;
    }

    /// Trace the scalar SELF/`this` fields of every native activation currently
    /// capable of crossing a safepoint.
    pub(crate) fn trace_native_jit_activations(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for activation in &self.jit_native_activations[..self.jit_native_activation_top] {
            for slot in [activation.self_slot, activation.this_slot] {
                if !slot.is_null() {
                    // SAFETY: publication requires the native context to remain
                    // live; `Value::trace_value_slots` updates its low-word GC
                    // offset in place when the referent moves.
                    unsafe { (&*slot.cast::<Value>()).trace_value_slots(visitor) };
                }
            }
        }
    }

    /// Address of synchronous reentry depth shared by framed and frameless JIT
    /// calls. Emitted code checks and updates it around native recursion.
    pub fn jit_sync_reentry_depth_ptr(&mut self) -> *mut u32 {
        &mut self.sync_reentry_depth
    }

    /// Effective synchronous reentry limit for emitted native calls.
    pub fn jit_sync_reentry_limit(&self) -> u32 {
        self.max_stack_depth
            .min(crate::run_control::DEFAULT_MAX_SYNC_REENTRY_DEPTH)
    }

    /// Address of the live array-index accessor protector. Compiled dense-array
    /// stores read this on every store attempt because a VM call inside the same
    /// compiled entry can invalidate the protector before a later store.
    pub fn jit_array_index_accessor_protector_ptr(&mut self) -> *const bool {
        &self.array_index_accessor_protector
    }

    /// Opaque heap pointer for native leaf runtime stubs.
    ///
    /// Compiled code may pass this to `LeafNoAlloc` ABI entries only. Those
    /// entries must not allocate, trigger GC, or retain the pointer.
    pub fn jit_gc_heap_ptr(&self) -> *const std::ffi::c_void {
        std::ptr::addr_of!(self.gc_heap).cast::<std::ffi::c_void>()
    }

    /// Base of the JIT-readable live collection method IC table.
    ///
    /// The pointer is stable until the table grows. Compiled entries must read
    /// the pointer from `JitCtx` for each entry/reentry, not retain it outside
    /// the dynamic compiled-call extent.
    pub fn jit_collection_method_ics_ptr(&self) -> *const jit::JitCollectionMethodIcSlot {
        self.jit_collection_method_ics.as_ptr()
    }

    /// Number of slots starting at [`Self::jit_collection_method_ics_ptr`].
    pub fn jit_collection_method_ics_len(&self) -> u32 {
        self.jit_collection_method_ics.len() as u32
    }

    /// Base of the flat direct-method inline-link table. Baseline code reads
    /// a slot by IC site to build the callee window and branch with no bridge; the
    /// pointer is refreshed into `JitCtx` on every entry/reentry because the table
    /// can grow (reallocate).
    pub fn jit_direct_method_inline_ptr(&self) -> *const JitDirectMethodInline {
        self.jit_direct_method_inline_slots.as_ptr()
    }

    /// Number of slots starting at [`Self::jit_direct_method_inline_ptr`].
    pub fn jit_direct_method_inline_len(&self) -> u32 {
        self.jit_direct_method_inline_slots.len() as u32
    }

    /// Capacity of the flat JIT register stack in slots — the overflow bound
    /// compiled code checks before reserving a callee window.
    #[must_use]
    pub fn jit_reg_stack_cap() -> usize {
        Self::REG_STACK_CAP
    }

    /// GC-trace the live JIT register stack (`reg_stack[0..reg_top]`): every
    /// callee window of an in-flight frameless JIT call. A no-op when no such
    /// call is in flight.
    pub(crate) fn trace_reg_stack(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        if std::env::var_os("OTTER_GC_VERIFY").is_some_and(|v| v != "0") {
            self.verify_reg_stack_roots();
        }
        for value in &self.reg_stack[..self.reg_top] {
            value.trace_value_slots(visitor);
        }
    }

    /// Diagnostic (`OTTER_GC_VERIFY=1`): scan the live register window
    /// `reg_stack[0..reg_top]` for any slot whose pointer target is not a
    /// plausible live object, and report its index and the current `reg_top`.
    /// Pinpoints stale register cells that a window-accounting slip left inside
    /// the traced range. Off by default; never mutates state.
    #[cold]
    pub(crate) fn verify_reg_stack_roots(&self) {
        let base = self.reg_stack.as_ptr();
        for (i, value) in self.reg_stack[..self.reg_top].iter().enumerate() {
            if let Some((size, tag, forwarded)) = value.debug_gc_target_header(&self.gc_heap) {
                // A forwarded target is fine — the trace pass will rewrite this
                // slot to the new location. Only a non-forwarded implausible
                // header is a genuinely stale/dangling register cell.
                if !forwarded && (size == 0 || size > (1u32 << 20) || tag == 0) {
                    eprintln!(
                        "OTTER_GC_VERIFY: stale reg_stack slot idx={i} reg_top={} base={base:p} \
                         target_size={size} target_tag={tag}",
                        self.reg_top
                    );
                }
            }
        }
    }

    #[inline]
    pub(crate) fn reclaim_registers(&mut self, frame: &mut Frame) {
        // A `Window` frame's registers live in the flat register stack; release
        // them by truncating the cursor back to the window base.
        if let Some(base) = frame.registers.window_base() {
            self.free_reg_window(base);
            return;
        }
        // An inline-owned, heap-spilled buffer goes back to the pool.
        if let crate::frame_state::FrameRegisters::Owned(regs) = &mut frame.registers
            && regs.spilled()
            && self.reg_pool.len() < Self::REG_POOL_CAP
        {
            let mut buf = std::mem::take(regs).into_vec();
            buf.clear();
            self.reg_pool.push(buf);
        }
    }

    /// Draw a reservation-stable [`HoltStack`] for a synchronous re-entry,
    /// reusing a pooled buffer when one is free so the per-stack reservation is
    /// not re-`malloc`ed on the hot callback path.
    #[inline]
    pub(crate) fn draw_stack(&mut self) -> HoltStack {
        self.holt_pool.pop().unwrap_or_default()
    }

    /// Return a drained re-entry stack to the pool for reuse. The stack is
    /// cleared (it holds no live frames, so the pool is never GC-traced); a full
    /// pool drops the stack instead.
    #[inline]
    pub(crate) fn return_stack(&mut self, mut stack: HoltStack) {
        while let Some(mut frame) = stack.pop() {
            self.frame_release_cold(&mut frame);
            self.reclaim_registers(&mut frame);
        }
        if self.holt_pool.len() < Self::HOLT_POOL_CAP {
            self.holt_pool.push(stack);
        }
    }

    /// Maximum pooled re-entry stacks retained at once.
    const HOLT_POOL_CAP: usize = 64;

    /// Maximum pooled register buffers retained at once.
    const REG_POOL_CAP: usize = 256;
    /// Largest pooled buffer capacity (in `Value`s) kept for reuse.
    const REG_POOL_MAX_CAP: usize = 4096;

    /// Acquire (or lazily create) this frame's cold side record and
    /// then return a mutable borrow.
    #[inline]
    pub(crate) fn frame_ensure_cold(&mut self, frame: &mut Frame) -> &mut cold_frame::ColdFrame {
        let idx = match frame.cold {
            Some(idx) => idx,
            None => {
                let idx = self.cold_frames.acquire();
                frame.cold = Some(idx);
                idx
            }
        };
        self.cold_frames.get_mut(idx)
    }

    /// Release `frame`'s cold record back to the pool if it holds one.
    /// Called when a frame is popped off the dispatcher stack.
    #[inline]
    pub(crate) fn frame_release_cold(&mut self, frame: &mut Frame) {
        if let Some(idx) = frame.cold.take() {
            self.cold_frames.release(idx);
        }
    }

    /// Detach `frame`'s cold record out of the pool, returning it as
    /// an owned [`Box`] so the caller can store it alongside the
    /// parked frame (async await, generator yield). Returns `None`
    /// when the frame had no cold state.
    #[inline]
    pub(crate) fn frame_detach_cold(
        &mut self,
        frame: &mut Frame,
    ) -> Option<Box<cold_frame::ColdFrame>> {
        let idx = frame.cold.take()?;
        Some(Box::new(self.cold_frames.detach(idx)))
    }

    /// Re-attach an owned cold record into the pool and bind it to
    /// `frame`. Matches [`Self::frame_detach_cold`] on the resume path.
    #[inline]
    pub(crate) fn frame_attach_cold(
        &mut self,
        frame: &mut Frame,
        cold: Box<cold_frame::ColdFrame>,
    ) {
        let idx = self.cold_frames.attach(*cold);
        frame.cold = Some(idx);
    }

    /// Borrow the per-interpreter cold-frame pool.
    #[inline]
    #[must_use]
    pub(crate) fn cold_frames(&self) -> &cold_frame::ColdFramePool {
        &self.cold_frames
    }

    /// Borrow the per-realm typed intrinsic slots.
    #[inline]
    #[must_use]
    pub(crate) fn realm_intrinsics(&self) -> &realm_intrinsics::RealmIntrinsics {
        &self.realm_intrinsics
    }
}

impl Interpreter {
    /// Pop the top frame and route its completion value.
    ///
    /// # Algorithm
    /// 1. If the popped frame was entered via `Op::New`, apply the
    ///    `OrdinaryConstruct` step-11 substitution: a non-object
    ///    return reuses the freshly allocated `this`.
    /// 2. If the popped frame is an **async** frame, settle its
    ///    `result_promise` as fulfilled with the resolved value
    ///    and drain the resulting reaction jobs into the
    ///    microtask queue. The caller's destination register was
    ///    populated with the promise at call entry, so we do not
    ///    write to it again. When the stack is now empty (an
    ///    async-resume mini-stack just finished) return
    ///    `Ok(Some(Undefined))` so the surrounding driver loop
    ///    exits cleanly; otherwise return `Ok(None)` to continue
    ///    in the caller frame.
    /// 3. For non-async frames, write the resolved value into the
    ///    caller's `return_register`. Top-of-stack `<main>` falls
    ///    through with `return_register = None` and surfaces the
    ///    completion as `Some(value)`.
    ///
    /// # Errors
    /// - [`VmError::InvalidOperand`] when the stack is empty or
    ///   the caller's return register is out of bounds.
    pub(crate) fn pop_frame(
        &mut self,
        stack: &mut HoltStack,
        value: Value,
    ) -> Result<Option<Value>, VmError> {
        let mut popped = stack.pop().ok_or_else(|| VmError::InvalidOperand)?;
        let construct_target = self.frame_cold(&popped).and_then(|c| c.construct_target);
        let is_derived_ctor = self
            .frame_cold(&popped)
            .is_some_and(|c| c.is_derived_constructor);
        let mut derived_this = popped.this_value;
        if derived_this.is_hole()
            && let Some(cell) = self.frame_cold(&popped).and_then(|c| c.derived_this_cell)
        {
            derived_this = crate::read_upvalue(&self.gc_heap, cell);
        }
        // Release the cold slot now so the pool can reuse it; the
        // remaining cold-record reads above already happened.
        self.frame_release_cold(&mut popped);
        // The frame is terminal — return its spilled register window to the
        // pool. Nothing below reads `popped.registers`.
        self.reclaim_registers(&mut popped);
        let resolved = if is_derived_ctor {
            // §10.2.2 derived-constructor return semantics. An object
            // return overrides the bound `this`; `undefined` yields
            // the `super(...)`-bound `this` (ReferenceError if
            // `super` never ran); any other primitive is a TypeError.
            if value.is_object_type() {
                value
            } else if value.is_undefined() {
                if derived_this.is_hole() {
                    return Err(self.err_this_uninit(( "must call super constructor in derived class before accessing 'this' or returning from derived constructor".to_string()).into()));
                }
                derived_this
            } else {
                return Err(self.err_type(
                    ("derived constructors may only return an object or undefined".to_string())
                        .into(),
                ));
            }
        } else {
            match construct_target {
                Some(_) if value.is_object_type() => value,
                Some(target) => Value::object(target),
                None => value,
            }
        };
        if let Some(state) = popped.async_state {
            crate::promise_dispatch::resolve_promise_from_interpreter(
                self,
                state.result_promise,
                resolved,
                None,
            )?;
            if stack.is_empty() {
                return Ok(Some(Value::undefined()));
            }
            return Ok(None);
        }
        let Some(return_reg) = popped.return_register else {
            return Ok(Some(resolved));
        };
        let caller = stack.last_mut().ok_or_else(|| VmError::InvalidOperand)?;
        write_register(caller, return_reg, resolved)?;
        // Caller's pc was set to the next instruction at call time;
        // nothing to advance here.
        Ok(None)
    }

    /// §14.15.3 — run the `finally` blocks between an abrupt `return` /
    /// `break` / `continue` and its target, then perform the
    /// completion. Pops handlers off the top frame until the handler
    /// stack reaches `floor`; the first `finally` found parks the
    /// completion (`pending_abrupt`) and jumps to the finally body —
    /// `Op::EndFinally` resumes this walk. With no remaining `finally`,
    /// a `Jump` sets the target pc and a `Return` pops the frame.
    pub(crate) fn unwind_abrupt(
        &mut self,
        stack: &mut HoltStack,
        completion: crate::cold_frame::AbruptKind,
        floor: u32,
    ) -> Result<Option<Value>, VmError> {
        use crate::cold_frame::AbruptKind;
        loop {
            let top_idx = stack.len() - 1;
            let handler_count = self
                .frame_cold(&stack[top_idx])
                .map(|c| c.handlers.len() as u32)
                .unwrap_or(0);
            if handler_count <= floor {
                return match completion {
                    AbruptKind::Jump(pc) => {
                        stack[top_idx].pc = pc;
                        Ok(None)
                    }
                    AbruptKind::Return(v) => self.pop_frame(stack, v),
                };
            }
            let handler = self
                .frame_cold_mut(&mut stack[top_idx])
                .and_then(|c| c.handlers.pop());
            // §14.15.3 — discard completions parked by `finally`
            // blocks this completion abandons (depth above the
            // remaining handler stack).
            if let Some(cold) = self.frame_cold_mut(&mut stack[top_idx]) {
                let len = cold.handlers.len() as u32;
                cold.parked_finally.retain(|(_, depth)| *depth <= len);
            }
            match handler {
                Some(h) if h.finally_pc.is_some() => {
                    let finally_pc = h.finally_pc.expect("finally_pc checked");
                    let cold = self.frame_ensure_cold(&mut stack[top_idx]);
                    let depth = cold.handlers.len() as u32;
                    cold.parked_finally.push((
                        crate::cold_frame::ParkedFinally::Abrupt(completion, floor),
                        depth,
                    ));
                    stack[top_idx].pc = finally_pc;
                    return Ok(None);
                }
                // Catch-only handler crossed by the abrupt completion:
                // pop it (cleanup) and keep walking.
                Some(_) => continue,
                None => {
                    return match completion {
                        AbruptKind::Jump(pc) => {
                            stack[top_idx].pc = pc;
                            Ok(None)
                        }
                        AbruptKind::Return(v) => self.pop_frame(stack, v),
                    };
                }
            }
        }
    }

    /// Return `value` from the top frame, first running any enclosing
    /// `finally` blocks (§14.15.3). Equivalent to [`Self::pop_frame`]
    /// when no `finally` handler is active.
    pub(crate) fn return_running_finally(
        &mut self,
        stack: &mut HoltStack,
        value: Value,
    ) -> Result<Option<Value>, VmError> {
        let top_idx = stack.len() - 1;
        let has_finally = self
            .frame_cold(&stack[top_idx])
            .is_some_and(|c| c.handlers.iter().any(|h| h.finally_pc.is_some()));
        if has_finally {
            self.unwind_abrupt(stack, crate::cold_frame::AbruptKind::Return(value), 0)
        } else {
            self.pop_frame(stack, value)
        }
    }
}
