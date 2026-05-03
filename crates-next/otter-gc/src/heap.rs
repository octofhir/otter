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
}

/// Orchestrator. Owned by the runtime; passed by `&mut` to every
/// allocation / barrier / GC call.
pub struct GcHeap {
    new_space: NewSpace,
    old_space: OldSpace,
    large_space: LargeObjectSpace,
    trace_table: TraceTable,
    marking: MarkingState,
    handle_stack: Box<HandleStack>,
    global_handles: Box<GlobalHandleTable>,
    stats: HeapStats,
}

impl GcHeap {
    /// Build a fresh heap. Initialises the cage with the default
    /// size if it has not been initialised already.
    ///
    /// # Errors
    ///
    /// Returns [`OutOfMemory`] if the cage cannot be initialised
    /// (cage exhausted or alloc failed).
    pub fn new() -> Result<Self, OutOfMemory> {
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
        })
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
        s.allocated_bytes = s.new_allocated_bytes + s.old_allocated_bytes;
        s.page_count = self.new_space.from_page_count() * 2
            + self.old_space.page_count()
            + self.large_space.page_count();
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
    /// - [`OutOfMemory::CageExhausted`] — the cage cannot satisfy
    ///   a fresh page request.
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
