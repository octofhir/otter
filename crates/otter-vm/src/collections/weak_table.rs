//! Identity hash table for `WeakMap` and `WeakSet`, inside the GC heap.
//!
//! A weak collection kept two Rust containers: a `Vec` of entries and an
//! `FxHashMap` identity index. Both are malloc memory a page image
//! cannot carry. Both now live in one GC body — a bucket array followed
//! by the entry array in the same cell, chains threaded through the
//! entries — the same shape [`super::table::OrderedTableBody`] gave
//! `Map` and `Set`.
//!
//! Two things make the weak variant its own type rather than a reuse:
//!
//! - **Entries are ephemerons.** The strong trace of this body is
//!   deliberately empty: a key must never be kept alive by the table
//!   that holds it, and a value only becomes live through the ephemeron
//!   fixpoint once its key is. The collection body's `ephemeron_via`
//!   hook reaches the entries through the table handle.
//! - **Every key hashes by identity**, and a moving collection rewrites
//!   the addresses the hashes derive from. The old `FxHashMap` index
//!   was invalidated from the ephemeron walk and rebuilt on the next
//!   access; the chains here work the same way — [`WeakTableBody::mark_stale`]
//!   from the walk, and the next lookup re-threads every bucket in
//!   place, no allocation. Weak collections are not iterable, so
//!   deletion compacts by swap-remove instead of tombstoning, which
//!   also just marks the chains stale.
//!
//! # Invariants
//!
//! - Only the first `len` entries are traced or walked. Trailing
//!   capacity is not guaranteed zeroed: a swept old-space cell arrives
//!   with whatever the previous tenant left.
//! - Bucket count is a power of two; a bucket index is `hash & mask`.
//! - `chains_stale` set ⇒ no lookup trusts `next`/buckets until
//!   [`WeakTableBody::rethread`] runs. Mutations that reorder entries
//!   (swap-remove, fixpoint prune) must set it.
//! - Old space: the table lives exactly as long as its collection, and
//!   entry slot addresses handed to the ephemeron visitor must not move
//!   during the walk.
//!
//! # See also
//!
//! - [`super::table`] — the strong-entry sibling.
//! - <https://tc39.es/ecma262/#sec-weakmap-objects>

use std::marker::PhantomData;

use otter_gc::raw::SlotVisitor;

use super::WeakCollectionKey;
use crate::Value;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for a WeakMap's table.
pub const WEAK_MAP_TABLE_BODY_TYPE_TAG: u8 = 0x39;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for a WeakSet's table.
pub const WEAK_SET_TABLE_BODY_TYPE_TAG: u8 = 0x3a;

/// End of a collision chain, and the value of an unused bucket.
const EMPTY: u32 = u32::MAX;

/// Marker giving a map table and a set table distinct type tags.
pub trait WeakTableKind: 'static {
    /// The table body's reserved type tag.
    const TABLE_TYPE_TAG: u8;
}

/// Marker for `WeakMap` tables.
pub struct MapKind;

impl WeakTableKind for MapKind {
    const TABLE_TYPE_TAG: u8 = WEAK_MAP_TABLE_BODY_TYPE_TAG;
}

/// Marker for `WeakSet` tables.
pub struct SetKind;

impl WeakTableKind for SetKind {
    const TABLE_TYPE_TAG: u8 = WEAK_SET_TABLE_BODY_TYPE_TAG;
}

/// One weak entry. The value slot is `undefined` in a set's table.
#[derive(Clone, Copy)]
pub struct WeakEntry {
    /// The weakly-held key.
    pub key: WeakCollectionKey,
    /// The value the key maps to.
    pub value: Value,
    /// Next entry in the same bucket, or [`EMPTY`]. Meaningless while
    /// the chains are stale.
    pub(crate) next: u32,
}

/// Handle to a weak collection's table.
pub type WeakTableHandle<K> = otter_gc::Gc<WeakTableBody<K>>;

/// Header for a weak identity table. The bucket array and the entry
/// array follow it in the same cell, in that order.
#[repr(C, align(8))]
pub struct WeakTableBody<K> {
    /// Entries the table can hold before it must grow.
    capacity: u32,
    /// Live entries. Deletion compacts, so there are no tombstones.
    len: u32,
    /// `bucket_count - 1`; bucket count is a power of two.
    bucket_mask: u32,
    /// Set when entry addresses may have moved (ephemeron walk) or
    /// entries were reordered (swap-remove, prune). Lookups re-thread
    /// the chains in place before trusting them.
    chains_stale: bool,
    _kind: PhantomData<K>,
}

impl<K: WeakTableKind> WeakTableBody<K> {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        for entry in self.entries() {
            crate::code_liveness::visit_value(&entry.value, visitor);
        }
    }

    /// Buckets a table of `capacity` entries uses.
    #[must_use]
    fn bucket_count_for(capacity: usize) -> usize {
        capacity.max(1).next_power_of_two()
    }

    /// Trailing bytes a table of `capacity` entries needs.
    #[must_use]
    pub fn trailing_bytes(capacity: usize) -> usize {
        Self::bucket_count_for(capacity) * std::mem::size_of::<u32>()
            + Self::entry_array_padding(capacity)
            + capacity * std::mem::size_of::<WeakEntry>()
    }

    /// Padding between the bucket array and the entry array, so the
    /// entries land on their own alignment.
    fn entry_array_padding(capacity: usize) -> usize {
        let buckets = Self::bucket_count_for(capacity) * std::mem::size_of::<u32>();
        let align = std::mem::align_of::<WeakEntry>();
        (align - (buckets % align)) % align
    }

    /// Header for an empty table sized for `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let buckets = Self::bucket_count_for(capacity);
        Self {
            capacity: u32::try_from(capacity).expect("weak table capacity exceeds u32"),
            len: 0,
            bucket_mask: u32::try_from(buckets - 1).expect("bucket count exceeds u32"),
            chains_stale: false,
            _kind: PhantomData,
        }
    }

    /// Entries the table can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    /// Live entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// `true` when the table holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn bucket_count(&self) -> usize {
        self.bucket_mask as usize + 1
    }

    fn buckets_ptr(&self) -> *mut u32 {
        // SAFETY: the allocation reserved the bucket array immediately
        // after this header.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>())
                .cast()
        }
    }

    fn entries_ptr(&self) -> *mut WeakEntry {
        let buckets = self.bucket_count() * std::mem::size_of::<u32>();
        let padding = Self::entry_array_padding(self.capacity());
        // SAFETY: the allocation reserved the entry array after the
        // header and the bucket array, padded onto its own alignment.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>() + buckets + padding)
                .cast()
        }
    }

    fn buckets_mut(&mut self) -> &mut [u32] {
        // SAFETY: the array holds exactly `bucket_count` words; every
        // read happens after `init_buckets` or a re-thread wrote them.
        unsafe { std::slice::from_raw_parts_mut(self.buckets_ptr(), self.bucket_count()) }
    }

    /// The live entries.
    #[must_use]
    pub fn entries(&self) -> &[WeakEntry] {
        // SAFETY: the first `len` records were written before the table
        // became reachable.
        unsafe { std::slice::from_raw_parts(self.entries_ptr().cast_const(), self.len()) }
    }

    /// The live entries, mutably.
    pub fn entries_mut(&mut self) -> &mut [WeakEntry] {
        // SAFETY: as in `entries`.
        unsafe { std::slice::from_raw_parts_mut(self.entries_ptr(), self.len()) }
    }

    /// Point every bucket at nothing. Called once, when the table is
    /// published, because trailing storage does not arrive zeroed.
    pub fn init_buckets(&mut self) {
        for bucket in self.buckets_mut() {
            *bucket = EMPTY;
        }
    }

    /// Declare every chain untrustworthy. Called from the ephemeron
    /// walk (relocation moved the addresses the hashes derive from) and
    /// from every mutation that reorders entries.
    pub fn mark_stale(&mut self) {
        self.chains_stale = true;
    }

    /// Rebuild every bucket chain from the current entry addresses.
    ///
    /// Linear, in place, no allocation — callable inside a payload
    /// borrow. Runs at most once per collection or reorder, which is
    /// what keeps the amortized lookup cost constant.
    fn rethread(&mut self) {
        for bucket in self.buckets_mut() {
            *bucket = EMPTY;
        }
        let mask = self.bucket_mask;
        for index in 0..self.len() {
            // SAFETY: `index < len`; the borrow of the entry ends before
            // the bucket write below.
            let hash = unsafe { (*self.entries_ptr().add(index)).key.identity_hash() };
            let bucket = (hash as u32 & mask) as usize;
            let head = self.buckets_mut()[bucket];
            // SAFETY: as above.
            unsafe { (*self.entries_ptr().add(index)).next = head };
            self.buckets_mut()[bucket] = index as u32;
        }
        self.chains_stale = false;
    }

    /// Position of the entry whose key matches `key`, re-threading the
    /// chains first when they are stale.
    #[must_use]
    pub fn position(&mut self, key: &WeakCollectionKey) -> Option<usize> {
        if self.chains_stale {
            self.rethread();
        }
        let bucket = (key.identity_hash() as u32 & self.bucket_mask) as usize;
        let mut current = self.buckets_mut()[bucket];
        while current != EMPTY {
            let entry = &self.entries()[current as usize];
            if entry.key.matches(key) {
                return Some(current as usize);
            }
            current = entry.next;
        }
        None
    }

    /// Append an entry, linking it into its bucket.
    ///
    /// The caller must have reserved capacity: growth allocates, and a
    /// payload borrow has no heap to allocate from.
    pub fn push(&mut self, key: WeakCollectionKey, value: Value) {
        let index = self.len();
        debug_assert!(
            index < self.capacity(),
            "weak table push without a reservation"
        );
        if self.chains_stale {
            self.rethread();
        }
        let bucket = (key.identity_hash() as u32 & self.bucket_mask) as usize;
        let head = self.buckets_mut()[bucket];
        // SAFETY: `index < capacity`, so the slot is inside the table.
        unsafe {
            self.entries_ptr().add(index).write(WeakEntry {
                key,
                value,
                next: head,
            });
        }
        self.len += 1;
        self.buckets_mut()[bucket] = index as u32;
    }

    /// Remove the entry at `index` by swapping the last entry into its
    /// place. Weak collections are not iterable, so no observer can see
    /// the reorder; the chains are re-threaded on the next lookup.
    pub fn swap_remove(&mut self, index: usize) {
        let last = self.len() - 1;
        if index != last {
            let moved = self.entries()[last];
            self.entries_mut()[index] = moved;
        }
        self.len -= 1;
        self.mark_stale();
    }

    /// Drop every entry whose key fails `keep`, compacting in place.
    /// Used by the ephemeron fixpoint to prune dead-key entries.
    pub fn retain(&mut self, mut keep: impl FnMut(&WeakEntry) -> bool) {
        let mut write = 0usize;
        for read in 0..self.len() {
            let entry = self.entries()[read];
            if keep(&entry) {
                if write != read {
                    self.entries_mut()[write] = entry;
                }
                write += 1;
            }
        }
        if write != self.len() {
            self.len = write as u32;
            self.mark_stale();
        }
    }

    /// Adopt another table's entries in order. Used by growth; the
    /// chains are built by the pushes.
    pub fn refill_from(&mut self, entries: &[WeakEntry]) {
        debug_assert!(self.is_empty());
        debug_assert!(entries.len() <= self.capacity());
        for entry in entries {
            self.push(entry.key, entry.value);
        }
    }
}

impl<K: WeakTableKind> otter_gc::SafeTraceable for WeakTableBody<K> {
    const TYPE_TAG: u8 = K::TABLE_TYPE_TAG;

    /// Deliberately empty: entries are ephemerons. Keys must not be
    /// kept alive by the table, and values become live only through
    /// the fixpoint once their key is. The collection body's
    /// `ephemeron_via` hook walks them through the handle.
    fn trace_slots_safe(&mut self, _visitor: &mut SlotVisitor<'_>) {}
}

/// The table payload behind `table`, or `None` for a null handle.
///
/// A collection body reaches its table this way while holding a payload
/// borrow, where there is no heap to ask, and the ephemeron machinery
/// reaches it the same way during a collection.
#[must_use]
pub fn body_of<K: WeakTableKind>(table: WeakTableHandle<K>) -> Option<*mut WeakTableBody<K>> {
    if table.is_null() {
        return None;
    }
    let header = table.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is a
    // `WeakTableBody<K>` one header past the start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<WeakTableBody<K>>()
    })
}

/// Allocate an empty weak table sized for `capacity` entries, in old
/// space, with the caller's roots live across the allocation.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub fn alloc_weak_table<K: WeakTableKind>(
    heap: &mut otter_gc::GcHeap,
    capacity: usize,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<WeakTableHandle<K>, otter_gc::OutOfMemory> {
    let table: WeakTableHandle<K> = heap.alloc_variable_with_roots(
        WeakTableBody::<K>::new(capacity),
        WeakTableBody::<K>::trailing_bytes(capacity),
        external_visit,
    )?;
    // Trailing storage is not guaranteed zeroed, and an unset bucket
    // must read as `EMPTY` rather than as entry zero.
    heap.with_payload(table, |body| {
        body.init_buckets();
        true
    });
    Ok(table)
}
