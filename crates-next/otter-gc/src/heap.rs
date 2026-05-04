//! Top-level GC heap — owns the spaces, the handle stacks, the
//! marking state, and the trace table.
//!
//! # Contents
//!
//! - [`GcHeap`] — the orchestrator the rest of the runtime sees.
//! - [`Roots`] — caller-supplied root sources for full GC.
//! - [`HeapStats`] — tiny snapshot of accounting (used by tests).
//!
//! # Invariants
//!
//! - Every public `alloc<T>` records `T::TYPE_TAG` on the
//!   [`crate::header::GcHeader`]; full GC dispatches through the
//!   trace table by tag.
//! - Black allocation: when a marking cycle is in progress
//!   (`marking.is_marking() == true`), new objects start black so
//!   the marker doesn't have to re-discover them. Phase-1 STW
//!   marker never observes the flag set during the mutator phase
//!   so the fast path costs one branch.
//! - Pages live forever inside the heap or are returned to the
//!   cage on full-GC sweep. Pages are never leaked across heap
//!   drops.
//!
//! # See also
//!
//! - GC architecture plan §6.1 (unsafe boundary).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::compressed::{Cage, Gc, RawGc, cage_base};
use crate::handle::{GlobalHandle, GlobalHandleTable, HandleStack};
use crate::header::{GcHeader, MarkColor};
use crate::marking::MarkingState;
use crate::oom::OutOfMemory;
use crate::page::{CELL_SIZE, align_up};
use crate::page::{LARGE_OBJECT_THRESHOLD, PAGE_HEADER_SIZE, Page, page_base_from_offset};
use crate::scavenger::ScavengeStats;
use crate::space::{LargeObjectSpace, NewSpace, OldSpace};
use crate::trace::{TraceTable, Traceable};

/// Type alias for the higher-order visitor closure used by the
/// GC: it receives a `&mut dyn FnMut(*mut RawGc)` slot visitor
/// and emits one slot pointer per outgoing reference. Hoisted out
/// of the public method signatures to keep clippy's
/// `type_complexity` lint quiet.
pub type RootSlotVisitor<'a> = dyn FnMut(&mut dyn FnMut(*mut RawGc)) + 'a;

/// Caller-supplied root source for full GC.
///
/// Wraps a slot-visit closure; the visitor argument it hands the
/// GC receives `*mut RawGc` slot pointers — the scavenger may
/// rewrite each in place when an object moves.
pub struct Roots<'a> {
    #[allow(dead_code)]
    pub(crate) slot_visit: &'a mut RootSlotVisitor<'a>,
}

impl<'a> Roots<'a> {
    /// Build a Roots from a slot-visit closure.
    pub fn new(slot_visit: &'a mut RootSlotVisitor<'a>) -> Self {
        Self { slot_visit }
    }

    /// Empty root set — every reachable object must come from a
    /// handle scope or a global handle.
    pub fn empty() -> EmptyRoots {
        EmptyRoots
    }
}

/// Trivial empty-roots implementation; useful in tests where all
/// reachable objects sit in handle scopes.
pub struct EmptyRoots;

/// Lightweight stats snapshot.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeapStats {
    /// Total bytes allocated across all spaces.
    pub allocated_bytes: usize,
    /// Pages owned by all spaces.
    pub page_count: usize,
    /// New-space allocated bytes.
    pub new_allocated_bytes: usize,
    /// Old-space allocated bytes.
    pub old_allocated_bytes: usize,
    /// Most recent scavenge result.
    pub last_scavenge: ScavengeStats,
    /// Bytes reclaimed by the most recent full GC.
    pub last_full_reclaimed: usize,
    /// Bytes currently tracked against the configured cap (sum
    /// of slot allocations + outstanding [`GcHeap::reserve_bytes`]
    /// reservations). `0` until the first allocation.
    pub tracked_bytes: u64,
    /// Configured heap cap in bytes (`0` = disabled).
    pub max_heap_bytes: u64,
}

/// Orchestrator. Owned by the runtime; passed by `&mut` to every
/// allocation / barrier / GC call.
///
/// Field order is **load-bearing for hot-path performance**:
/// `max_heap_bytes` and `tracked_bytes` lead so the alloc cap
/// check shares a cache line with `new_space`'s bump cursor,
/// avoiding a second cache miss in the steady-state allocation
/// loop.
pub struct GcHeap {
    /// Configured per-heap soft cap (`0` = disabled). Front-of-
    /// struct so the alloc fast-path's "is the cap enabled?"
    /// check shares a cache line with [`Self::new_space`].
    max_heap_bytes: u64,
    /// Bytes currently tracked against [`Self::max_heap_bytes`].
    /// Sum of slot allocations + [`Self::reserve_bytes`]
    /// reservations, recomputed from the live spaces after every
    /// emergency full GC. Always `0` for an empty heap.
    tracked_bytes: u64,
    /// Outstanding [`Self::reserve_bytes`] reservations. Tracked
    /// separately from slot allocations so emergency-GC
    /// reconciliation does not lose them.
    reserved_bytes: u64,
    new_space: NewSpace,
    old_space: OldSpace,
    large_space: LargeObjectSpace,
    trace_table: TraceTable,
    marking: MarkingState,
    handle_stack: Box<HandleStack>,
    global_handles: Box<GlobalHandleTable>,
    stats: HeapStats,
    /// Cooperative-cancellation flag; flipped to `true` when the
    /// cap rejects an allocation. Watchdogs may poll this between
    /// safepoints to short-circuit, but the **primary** OOM signal
    /// is the `Err(OutOfMemory)` returned from [`Self::alloc`] —
    /// the alloc is **never** materialised on a cap miss
    /// (architecture plan §2.1 caveat).
    oom_flag: Arc<AtomicBool>,
}

impl GcHeap {
    /// Build a fresh heap with no cap. Equivalent to
    /// `Self::with_max_heap_bytes(0)`.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfMemory`] if the cage cannot be initialised
    /// (cage exhausted or alloc failed).
    pub fn new() -> Result<Self, OutOfMemory> {
        Self::with_max_heap_bytes(0)
    }

    /// Build a fresh heap honouring a per-heap byte cap.
    ///
    /// `cap == 0` disables the cap; allocations succeed until the
    /// cage is exhausted. `cap > 0` is **load-bearing**: an
    /// allocation that would overshoot the cap triggers one
    /// emergency full GC and, if the cap is still exceeded, is
    /// refused with [`OutOfMemory::HeapCapExceeded`]. The slot is
    /// never materialised (architecture plan §2.1 caveat).
    ///
    /// # Errors
    ///
    /// Returns [`OutOfMemory`] when the cage cannot satisfy the
    /// initial new-space.
    pub fn with_max_heap_bytes(cap: u64) -> Result<Self, OutOfMemory> {
        Cage::ensure_default().map_err(|_| OutOfMemory::CageExhausted)?;
        let new_space = NewSpace::new(crate::space::DEFAULT_NEW_SPACE_PAGES)?;
        Ok(Self {
            new_space,
            old_space: OldSpace::new(),
            large_space: LargeObjectSpace::new(),
            trace_table: TraceTable::new(),
            marking: MarkingState::new(),
            handle_stack: Box::new(HandleStack::new()),
            global_handles: Box::new(GlobalHandleTable::new()),
            stats: HeapStats::default(),
            max_heap_bytes: cap,
            tracked_bytes: 0,
            reserved_bytes: 0,
            oom_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Configured per-heap cap in bytes (`0` = disabled).
    pub fn max_heap_bytes(&self) -> u64 {
        self.max_heap_bytes
    }

    /// Bytes currently tracked against the cap (slot allocations
    /// + outstanding [`Self::reserve_bytes`] reservations).
    pub fn tracked_bytes(&self) -> u64 {
        self.tracked_bytes
    }

    /// Cooperative-cancellation OOM flag. Cloned cheaply; safe to
    /// share with watchdogs / interrupt threads. Never the
    /// **primary** OOM signal — see [`Self::alloc`].
    pub fn oom_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.oom_flag)
    }

    /// Reserve `bytes` against the cap without materialising a
    /// slot (off-slot accounting — e.g. `Vec` capacity inside a
    /// payload).
    ///
    /// On overshoot: one emergency full GC, then retry once. If
    /// still over → [`OutOfMemory::HeapCapExceeded`] and the
    /// reservation is **not** booked.
    ///
    /// # Errors
    ///
    /// [`OutOfMemory::HeapCapExceeded`] when the cap is enabled
    /// and the reservation cannot be satisfied even after an
    /// emergency full GC.
    pub fn reserve_bytes(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
        if self.max_heap_bytes == 0 {
            return Ok(());
        }
        self.account_or_collect(bytes)?;
        self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
        Ok(())
    }

    /// Release `bytes` previously booked via [`Self::reserve_bytes`].
    /// Saturates at zero if the caller overshoots — not an error;
    /// keeps the counter monotone-correct under arithmetic edge
    /// cases.
    pub fn release_bytes(&mut self, bytes: u64) {
        if self.max_heap_bytes == 0 {
            return;
        }
        let actual = bytes.min(self.reserved_bytes);
        self.reserved_bytes = self.reserved_bytes.saturating_sub(actual);
        self.tracked_bytes = self.tracked_bytes.saturating_sub(actual);
    }

    /// Account `bytes` against the cap. Outlined so the alloc
    /// hot path adds only a single cap-enabled branch when the
    /// cap is disabled. On overshoot: one emergency full GC,
    /// retry once, otherwise refuse.
    ///
    /// Caller pre-checks `self.max_heap_bytes != 0`; this
    /// function is not invoked when the cap is disabled, so the
    /// disabled-cap branch never executes here.
    #[cold]
    #[inline(never)]
    fn account_or_collect(&mut self, bytes: u64) -> Result<(), OutOfMemory> {
        let cap = self.max_heap_bytes;
        let projected = self.tracked_bytes.saturating_add(bytes);
        if projected <= cap {
            self.tracked_bytes = projected;
            return Ok(());
        }
        // External root visitor is empty — the handle stack and
        // global handle table are walked from inside
        // `collect_full`, and the embedder has no opportunity to
        // re-enter the heap during a refused allocation.
        let mut noop = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        self.collect_full(&mut noop);
        self.tracked_bytes = self.live_bytes_total().saturating_add(self.reserved_bytes);
        let projected = self.tracked_bytes.saturating_add(bytes);
        if projected <= cap {
            self.tracked_bytes = projected;
            return Ok(());
        }
        self.oom_flag.store(true, Ordering::Relaxed);
        Err(OutOfMemory::HeapCapExceeded {
            requested_bytes: bytes,
            heap_limit_bytes: cap,
        })
    }

    /// Sum of allocated bytes across new + old + LOS. Counts
    /// post-collect retained-page slack as well as live objects;
    /// strict liveness arrives with task 86's incremental sweep.
    fn live_bytes_total(&self) -> u64 {
        let new = self.new_space.allocated_bytes() as u64;
        let old = self.old_space.allocated_bytes() as u64;
        let los = self.large_space.allocated_bytes() as u64;
        new.saturating_add(old).saturating_add(los)
    }

    /// Register a [`Traceable`] type so allocations of `T` can be
    /// traced during marking and scavenging.
    pub fn register_traceable<T: Traceable>(&mut self) {
        self.trace_table.register::<T>();
    }

    /// Reference to the trace table (used by tests).
    pub fn trace_table(&self) -> &TraceTable {
        &self.trace_table
    }

    /// Borrow the heap's handle stack.
    pub fn handle_stack(&self) -> &HandleStack {
        &self.handle_stack
    }

    /// Raw pointer to the heap's handle stack. Used by
    /// [`HandleScope::from_ptr`] to open a scope without
    /// holding an immutable borrow on the heap — the heap can
    /// continue to be mutated (`alloc`, `collect_*`,
    /// `write_barrier`) while the scope is open. The Box-owned
    /// stack has a stable address that lives as long as the
    /// `GcHeap`.
    pub fn handle_stack_ptr(&self) -> *const HandleStack {
        &*self.handle_stack as *const _
    }

    /// Borrow the heap's global handle table.
    pub fn global_handles(&self) -> &GlobalHandleTable {
        &self.global_handles
    }

    /// Raw pointer to the heap's global handle table. See
    /// [`Self::handle_stack_ptr`] for the rationale.
    pub fn global_handles_ptr(&self) -> *const GlobalHandleTable {
        &*self.global_handles as *const _
    }

    /// Reference to the marking state (Phase 2 / task 86 will
    /// drive it).
    pub fn marking(&self) -> &MarkingState {
        &self.marking
    }

    /// Mutable marking state (Phase 2).
    pub fn marking_mut(&mut self) -> &mut MarkingState {
        &mut self.marking
    }

    /// Stats snapshot.
    pub fn stats(&self) -> HeapStats {
        let mut s = self.stats;
        s.new_allocated_bytes = self.new_space.allocated_bytes();
        s.old_allocated_bytes = self.old_space.allocated_bytes();
        s.allocated_bytes =
            s.new_allocated_bytes + s.old_allocated_bytes + self.large_space.allocated_bytes();
        s.page_count = self.new_space.from_page_count() * 2
            + self.old_space.page_count()
            + self.large_space.page_count();
        s.tracked_bytes = self.tracked_bytes;
        s.max_heap_bytes = self.max_heap_bytes;
        s
    }

    /// Allocate a `T` on the GC heap.
    ///
    /// The header gets `T::TYPE_TAG`; the value is moved into the
    /// payload area. When marking is active the new object starts
    /// black (V8 black-allocation since 2018).
    ///
    /// # Errors
    ///
    /// - [`OutOfMemory::HeapCapExceeded`] — the configured per-heap
    ///   cap was exceeded and an emergency full GC could not free
    ///   enough room. The slot is **not** materialised.
    /// - [`OutOfMemory::CageExhausted`] — the cage cannot satisfy
    ///   a fresh page request.
    #[inline]
    pub fn alloc<T: Traceable>(&mut self, value: T) -> Result<Gc<T>, OutOfMemory> {
        // Lazy register so callers never forget. Idempotent.
        if self.trace_table.get(T::TYPE_TAG).is_none() {
            self.trace_table.register::<T>();
        }
        let total = std::mem::size_of::<GcHeader>() + std::mem::size_of::<T>();
        let aligned = align_up(total, CELL_SIZE);
        debug_assert!(
            aligned <= u32::MAX as usize,
            "object size exceeds u32 limit"
        );

        // Cap check — runs before any slot is carved (architecture
        // §2.1 caveat). When the cap is disabled (`max_heap_bytes
        // == 0`), the hot path adds one load + branch and skips
        // accounting entirely; the cap-enabled path is outlined
        // under [`Self::account_or_collect`].
        if self.max_heap_bytes != 0 {
            self.account_or_collect(aligned as u64)?;
        }

        let is_marking = self.marking.is_marking();

        let offset = if aligned > LARGE_OBJECT_THRESHOLD {
            self.large_space.alloc(aligned)?
        } else {
            // Try young-gen first; if full, scavenge then retry.
            match self.new_space.alloc(aligned) {
                Some(off) => off,
                None => {
                    // Trigger scavenge with the empty external
                    // root visitor (handle stack + globals are
                    // walked internally).
                    let empty: fn(&mut dyn FnMut(*mut RawGc)) = |_| {};
                    self.collect_minor_internal(&mut |v| empty(v));
                    self.new_space
                        .alloc(aligned)
                        .ok_or(OutOfMemory::CageExhausted)?
                }
            }
        };

        // Initialise header.
        // SAFETY: offset is a fresh in-cage allocation; pointer
        // arithmetic preserves provenance through the cage's
        // base pointer.
        let header_ptr = unsafe { cage_base().add(offset as usize) as *mut GcHeader };
        let payload_ptr =
            unsafe { cage_base().add(offset as usize + std::mem::size_of::<GcHeader>()) as *mut T };
        // SAFETY: offset references a freshly-alloc-ed cage
        // region inside a page we own; bytes are zeroed by
        // alloc_zeroed at cage init.
        unsafe {
            let header = if aligned > LARGE_OBJECT_THRESHOLD {
                let h = GcHeader::new(T::TYPE_TAG, aligned as u32);
                if is_marking {
                    h.set_mark_color(MarkColor::Black);
                }
                h
            } else if is_marking {
                GcHeader::new_young_black(T::TYPE_TAG, aligned as u32)
            } else {
                GcHeader::new_young(T::TYPE_TAG, aligned as u32)
            };
            std::ptr::write(header_ptr, header);
            std::ptr::write(payload_ptr, value);
        }
        // SAFETY: offset is the cage offset of a freshly-alloc-ed
        // T payload; type tag matches `T::TYPE_TAG`.
        Ok(unsafe { Gc::from_offset(offset) })
    }

    /// Run a minor GC (Cheney scavenge).
    pub fn collect_minor(&mut self, _roots: EmptyRoots) {
        let empty: fn(&mut dyn FnMut(*mut RawGc)) = |_| {};
        self.collect_minor_internal(&mut |v| empty(v));
    }

    /// Run a minor GC with caller-supplied root visitor.
    pub fn collect_minor_with_roots(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        self.collect_minor_internal(external_visit);
    }

    fn collect_minor_internal(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        // Combine the caller's external_visit with the heap's
        // own handle-stack and global-handles walk.
        let handle_stack: *const HandleStack = &*self.handle_stack;
        let global_handles: *const GlobalHandleTable = &*self.global_handles;
        let mut combined = move |visitor: &mut dyn FnMut(*mut RawGc)| {
            // SAFETY: STW pause; raw pointers reconstituted to
            // shared references.
            unsafe {
                (*handle_stack).visit_slots(visitor);
                (*global_handles).visit_slots(visitor);
            }
            external_visit(visitor);
        };
        // SAFETY: STW pause for the duration of the call;
        // every type tag in from-space is registered.
        let stats = unsafe {
            crate::scavenger::scavenge(
                &mut self.new_space,
                &mut self.old_space,
                &self.trace_table,
                &[],
                &mut combined,
            )
        };
        self.stats.last_scavenge = stats;
    }

    /// Run a full GC (young-gen scavenge + old-gen mark-sweep).
    ///
    /// `external_visit` is invoked once for marking; it must
    /// yield every external root slot.
    pub fn collect_full(&mut self, external_visit: &mut RootSlotVisitor<'_>) {
        // 1) Scavenge first so survivors are in old / to-space.
        self.collect_minor_internal(external_visit);

        // 2) Reset old-space + LOS live counters; mark cycle.
        self.old_space.reset_live_bytes();
        self.large_space.reset_live_bytes();
        // Reset every header's mark to white before we begin.
        // SAFETY: STW pause + we own the spaces.
        unsafe {
            for page in self.old_space.pages() {
                page.for_each_object(|h, _| {
                    (*h).clear_mark();
                });
            }
            for page in self.large_space.pages() {
                page.for_each_object(|h, _| {
                    (*h).clear_mark();
                });
            }
            // Young-gen survivors after scavenge live in
            // to-space — but the scavenger has flipped, so they
            // now live in from-space. Clear marks there too.
            for page in self.new_space.from_pages() {
                page.for_each_object(|h, _| {
                    (*h).clear_mark();
                });
            }
        }

        self.marking.start_cycle();

        // 3) Shade roots.
        let handle_stack: *const HandleStack = &*self.handle_stack;
        let global_handles: *const GlobalHandleTable = &*self.global_handles;
        let marking_ptr = &mut self.marking as *mut MarkingState;
        let mut shade = move |slot: *mut RawGc| {
            // SAFETY: STW pause; valid slot per visitor contract.
            unsafe { (*marking_ptr).shade_from_slot(slot) };
        };
        // SAFETY: STW pause.
        unsafe {
            (*handle_stack).visit_slots(&mut shade);
            (*global_handles).visit_slots(&mut shade);
        }
        external_visit(&mut shade);

        // 4) Drain.
        // SAFETY: STW pause; all pushed headers alive.
        unsafe {
            self.marking.drain_full(&self.trace_table);
        }

        // 5) Sweep — anything still white in old / large / young
        // is dead. For old-space, walk pages; reap pages whose
        // live_bytes is zero. For young, our scavenger already
        // ran; any white survivor in from-space (post-flip) is
        // garbage we can drop at next scavenge but right now we
        // simply reset the whole thing — the next mutator alloc
        // path needs an empty from-space.
        // SAFETY: STW pause.
        let mut reclaimed = 0usize;
        unsafe {
            // Drop in-place + free per dead old-space object.
            for page in self.old_space.pages() {
                page.for_each_object(|h, _| {
                    if !(*h).is_marked() {
                        if let Some(drop_fn) = self.trace_table.get_drop((*h).type_tag()) {
                            drop_fn(h);
                        }
                        reclaimed += (*h).size_bytes() as usize;
                    }
                });
            }
            for page in self.large_space.pages() {
                page.for_each_object(|h, _| {
                    if !(*h).is_marked() {
                        if let Some(drop_fn) = self.trace_table.get_drop((*h).type_tag()) {
                            drop_fn(h);
                        }
                        reclaimed += (*h).size_bytes() as usize;
                    }
                });
            }
        }
        // Reap pages whose live bytes is zero.
        let _ = self.old_space.reap_dead_pages();
        let _ = self.large_space.reap_dead_pages();

        self.marking.finish_cycle();
        self.stats.last_full_reclaimed = reclaimed;
    }

    /// Construct a [`GlobalHandle`] from a `Gc<T>`. Note: the
    /// returned handle holds a raw pointer to the heap's global
    /// handle table; it must be dropped before the [`GcHeap`].
    pub fn create_global<T: ?Sized>(&self, gc: Gc<T>) -> GlobalHandle<T> {
        self.global_handles.create(gc)
    }

    /// Single hot-path write barrier for a pointer-field store.
    ///
    /// Caller must:
    /// - have already performed the underlying store
    ///   `(*slot_addr) = child.raw()`,
    /// - pass the parent's `Gc<T>` (so the heap can locate the
    ///   parent's header),
    /// - pass the address of the slot inside the parent (so the
    ///   barrier can compute the card-bit position).
    pub fn write_barrier<T: ?Sized, U: ?Sized>(
        &mut self,
        parent: Gc<T>,
        slot_addr: *mut Gc<U>,
        child: Gc<U>,
    ) {
        if parent.is_null() {
            return;
        }
        let parent_header = parent.as_header_ptr();
        // SAFETY: parent is non-null; slot_addr is inside the
        // parent payload as required by the barrier contract.
        unsafe {
            crate::barrier::write_barrier(
                parent_header,
                slot_addr as *mut u8,
                child.raw(),
                &mut self.marking,
            );
        }
    }

    /// Iterate every live object in every space (debug / snapshot
    /// hook). Used by [`crate::devtools_snapshot`] and tests.
    ///
    /// # Safety
    ///
    /// Must run under STW pause; visitor must not allocate.
    pub unsafe fn for_each_live_object<F>(&self, mut visitor: F)
    where
        F: FnMut(*mut GcHeader),
    {
        // SAFETY: STW pause + valid pages.
        unsafe {
            for page in self.new_space.from_pages() {
                page.for_each_object(|h, _| visitor(h));
            }
            for page in self.old_space.pages() {
                page.for_each_object(|h, _| visitor(h));
            }
            for page in self.large_space.pages() {
                page.for_each_object(|h, _| visitor(h));
            }
        }
    }

    /// Trace the slots of a single object — exposed for the
    /// snapshot writer.
    ///
    /// # Safety
    ///
    /// `header` is a valid live header registered in the trace
    /// table.
    pub unsafe fn trace_one(&self, header: *mut GcHeader, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: per docstring.
        unsafe {
            self.trace_table.trace(header, visitor);
        }
    }

    /// Page-base lookup helper used by the snapshot.
    pub fn page_base_of(offset: u32) -> *mut u8 {
        page_base_from_offset(offset)
    }

    /// Page-header-size helper.
    pub fn page_header_size() -> usize {
        PAGE_HEADER_SIZE
    }
}

impl std::fmt::Debug for GcHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcHeap")
            .field("new_pages", &self.new_space.from_page_count())
            .field("old_pages", &self.old_space.page_count())
            .field("large_pages", &self.large_space.page_count())
            .field("handles", &self.handle_stack.len())
            .field("is_marking", &self.marking.is_marking())
            .finish()
    }
}

// Drop: pages are owned by the spaces; their Drop returns them
// to the cage automatically.

// Re-export the page-base helper.
#[allow(dead_code)]
fn _page_base_check(p: &Page) -> *mut u8 {
    p.base_ptr()
}
