//! Insertion-ordered hash table for `Map` and `Set`, inside the GC heap.
//!
//! A collection kept two Rust containers: a `Vec` of entries and an
//! `FxHashMap` index from key hash to entry indices. Both are malloc
//! memory the collector does not own, so a page image could not carry
//! them, a restored copy would alias the original buffers, and the
//! tracer handed the collector slot addresses outside the heap.
//!
//! Both now live in one GC body — a bucket array followed by the entry
//! array, in the same cell, with collision chains threaded through the
//! entries themselves. That is V8's `OrderedHashTable`, and it is the
//! same shape for the same reason: one object, no side allocation, and
//! insertion order falls out of the entry array being append-only.
//!
//! # Contents
//!
//! - [`TableEntry`] — what `Map` and `Set` entries must provide.
//! - [`OrderedTableBody`] — the header; buckets and entries follow it.
//! - [`alloc_table`] — allocate one, with the caller's roots live.
//! - [`EMPTY`] — the end-of-chain / empty-bucket sentinel.
//!
//! # Invariants
//!
//! - Entries are append-only. A delete tombstones in place, so an index
//!   handed out once names the same entry for the table's life — which is
//!   what lets live iterators observe later additions.
//! - `len` counts appended entries, tombstones included, and is the only
//!   prefix that is traced. Capacity past it is not guaranteed to be
//!   zero: a swept cell from the old-space free list arrives with
//!   whatever the previous tenant left in it.
//! - Bucket count is a power of two and at least one, so a bucket index
//!   is `hash & bucket_mask`.
//! - Only keys that hash are chained. A key whose hash moves under GC
//!   (symbol, object identity) is appended unchained and found by the
//!   caller's linear scan — the same split the `FxHashMap` index made.
//! - Old space: a table lives as long as the collection that owns it, so
//!   a semispace copy of one is pure overhead, and an old-space body does
//!   not move.
//!
//! # See also
//!
//! - [`crate::array::element_slab`] — the same shape for dense elements.

use std::marker::PhantomData;

use otter_gc::raw::SlotVisitor;

/// End of a collision chain, and the value of an unused bucket.
pub const EMPTY: u32 = u32::MAX;

/// What the table needs to know about the records it stores.
pub trait TableEntry: Sized + 'static {
    /// Reserved [`otter_gc::Traceable::TYPE_TAG`] for a table of these.
    const TABLE_TYPE_TAG: u8;

    /// Visit every GC reference this entry holds.
    fn trace_entry(&mut self, visitor: &mut SlotVisitor<'_>);

    /// Hash of this entry's key, or `None` when the key is not
    /// chainable and must be found by linear scan.
    fn entry_hash(&self) -> Option<u64>;

    /// Index of the next entry in the same bucket, or [`EMPTY`].
    fn next(&self) -> u32;

    /// Set the next-in-bucket index.
    fn set_next(&mut self, next: u32);

    /// A record that occupies a slot but holds nothing.
    fn vacant() -> Self;
}

/// Header for an insertion-ordered hash table. The bucket array and the
/// entry array follow it in the same cell, in that order.
#[repr(C, align(8))]
pub struct OrderedTableBody<E> {
    /// Entries the table can hold before it must grow.
    capacity: u32,
    /// Entries appended so far, tombstones included.
    len: u32,
    /// `bucket_count - 1`; bucket count is a power of two.
    bucket_mask: u32,
    _entry: PhantomData<E>,
}

impl<E: TableEntry> OrderedTableBody<E> {
    /// Buckets a table of `capacity` entries uses.
    ///
    /// One bucket per entry, rounded up to a power of two, so a chain
    /// stays short without the table spending more on buckets than on
    /// the entries themselves.
    #[must_use]
    pub fn bucket_count_for(capacity: usize) -> usize {
        capacity.max(1).next_power_of_two()
    }

    /// Trailing bytes a table of `capacity` entries needs.
    #[must_use]
    pub fn trailing_bytes(capacity: usize) -> usize {
        Self::bucket_count_for(capacity) * std::mem::size_of::<u32>()
            + Self::entry_array_offset_padding(capacity)
            + capacity * std::mem::size_of::<E>()
    }

    /// Bytes of padding between the bucket array and the entry array, so
    /// the entries land on their own alignment.
    fn entry_array_offset_padding(capacity: usize) -> usize {
        let buckets = Self::bucket_count_for(capacity) * std::mem::size_of::<u32>();
        let align = std::mem::align_of::<E>();
        (align - (buckets % align)) % align
    }

    /// Header for an empty table sized for `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let buckets = Self::bucket_count_for(capacity);
        Self {
            capacity: u32::try_from(capacity).expect("table capacity exceeds u32"),
            len: 0,
            bucket_mask: u32::try_from(buckets - 1).expect("bucket count exceeds u32"),
            _entry: PhantomData,
        }
    }

    /// Entries the table can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    /// Entries appended so far, tombstones included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// `true` when nothing has been appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Buckets this table has.
    #[must_use]
    pub fn bucket_count(&self) -> usize {
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

    fn entries_ptr(&self) -> *mut E {
        let buckets = self.bucket_count() * std::mem::size_of::<u32>();
        let padding = Self::entry_array_offset_padding(self.capacity());
        // SAFETY: the allocation reserved the entry array after the
        // header and the bucket array, with `padding` bytes between so
        // the entries are aligned.
        unsafe {
            (self as *const Self as *mut u8)
                .add(std::mem::size_of::<Self>() + buckets + padding)
                .cast()
        }
    }

    /// The bucket array.
    #[must_use]
    pub fn buckets(&self) -> &[u32] {
        // SAFETY: the array holds exactly `bucket_count` words, written
        // to `EMPTY` when the table was published.
        unsafe { std::slice::from_raw_parts(self.buckets_ptr().cast_const(), self.bucket_count()) }
    }

    fn buckets_mut(&mut self) -> &mut [u32] {
        // SAFETY: as in `buckets`; `&mut self` rules out an aliasing read.
        unsafe { std::slice::from_raw_parts_mut(self.buckets_ptr(), self.bucket_count()) }
    }

    /// The appended entries, tombstones included.
    #[must_use]
    pub fn entries(&self) -> &[E] {
        // SAFETY: the first `len` records were written before the table
        // became reachable.
        unsafe { std::slice::from_raw_parts(self.entries_ptr().cast_const(), self.len()) }
    }

    /// The appended entries, mutably.
    pub fn entries_mut(&mut self) -> &mut [E] {
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

    /// Append `entry` and return its index, linking it into its bucket
    /// when its key hashes.
    ///
    /// The caller must have reserved capacity: growth allocates, and a
    /// payload borrow has no heap to allocate from.
    pub fn push(&mut self, mut entry: E) -> usize {
        let index = self.len();
        debug_assert!(index < self.capacity(), "table push without a reservation");
        let hash = entry.entry_hash();
        let head = match hash {
            Some(hash) => self.bucket_head(hash),
            None => EMPTY,
        };
        entry.set_next(head);
        // SAFETY: `index < capacity`, so the slot is inside the table.
        unsafe { self.entries_ptr().add(index).write(entry) };
        self.len += 1;
        if let Some(hash) = hash {
            let bucket = self.bucket_of(hash);
            self.buckets_mut()[bucket] = index as u32;
        }
        index
    }

    fn bucket_of(&self, hash: u64) -> usize {
        (hash as u32 & self.bucket_mask) as usize
    }

    /// First entry index in `hash`'s bucket, or [`EMPTY`].
    #[must_use]
    pub fn bucket_head(&self, hash: u64) -> u32 {
        self.buckets()[self.bucket_of(hash)]
    }

    /// Unlink the entry at `index` from `hash`'s chain.
    ///
    /// The entry itself stays where it is — it is tombstoned in place, so
    /// its index remains valid for live iterators.
    pub fn unlink(&mut self, hash: u64, index: usize) {
        let bucket = self.bucket_of(hash);
        let head = self.buckets()[bucket];
        if head == EMPTY {
            return;
        }
        if head as usize == index {
            let next = self.entries()[index].next();
            self.buckets_mut()[bucket] = next;
            return;
        }
        let mut current = head as usize;
        loop {
            let next = self.entries()[current].next();
            if next == EMPTY {
                return;
            }
            if next as usize == index {
                let after = self.entries()[index].next();
                self.entries_mut()[current].set_next(after);
                return;
            }
            current = next as usize;
        }
    }

    /// Drop every entry and empty every bucket.
    pub fn clear(&mut self) {
        for entry in self.entries_mut() {
            *entry = E::vacant();
        }
        self.len = 0;
        self.init_buckets();
    }

    /// Adopt `entries` as this table's contents, rebuilding the chains.
    ///
    /// Used by growth: the replacement table starts empty and takes the
    /// old one's records in order, so indices are preserved.
    pub fn refill_from(&mut self, entries: &[E])
    where
        E: Clone,
    {
        debug_assert!(self.is_empty());
        debug_assert!(entries.len() <= self.capacity());
        for entry in entries {
            self.push(entry.clone());
        }
    }
}

impl<E: TableEntry> otter_gc::SafeTraceable for OrderedTableBody<E> {
    const TYPE_TAG: u8 = E::TABLE_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        for entry in self.entries_mut() {
            entry.trace_entry(visitor);
        }
    }
}

/// Handle to a collection's table.
pub type TableHandle<E> = otter_gc::Gc<OrderedTableBody<E>>;

/// The table payload behind `table`, or `None` for a null handle.
///
/// A collection body reaches its table this way while holding a payload
/// borrow, where there is no heap to ask.
#[must_use]
pub fn body_of<E: TableEntry>(table: TableHandle<E>) -> Option<*mut OrderedTableBody<E>> {
    if table.is_null() {
        return None;
    }
    let header = table.as_header_ptr();
    // SAFETY: a non-null handle names a live cell whose payload is an
    // `OrderedTableBody<E>` one header past the start.
    Some(unsafe {
        header
            .cast::<u8>()
            .add(std::mem::size_of::<otter_gc::GcHeader>())
            .cast::<OrderedTableBody<E>>()
    })
}

/// Allocate an empty table sized for `capacity` entries.
///
/// `external_visit` must yield every root the caller holds: this
/// allocation can collect, and the collection waiting to receive the
/// table is exactly the kind of handle a collection would otherwise move
/// out from under the caller.
///
/// # Errors
/// Propagates [`otter_gc::OutOfMemory`].
pub fn alloc_table<E: TableEntry>(
    heap: &mut otter_gc::GcHeap,
    capacity: usize,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<TableHandle<E>, otter_gc::OutOfMemory> {
    let table: TableHandle<E> = heap.alloc_variable_with_roots(
        OrderedTableBody::<E>::new(capacity),
        OrderedTableBody::<E>::trailing_bytes(capacity),
        external_visit,
    )?;
    // Trailing storage is not guaranteed zeroed, and an unset bucket must
    // read as `EMPTY` rather than as entry zero.
    heap.with_payload(table, |body| {
        body.init_buckets();
        true
    });
    Ok(table)
}
