//! The isolate's property-name and hidden-class state, owned and rooted by its heap.
//!
//! A property name's identity, its atom, is minted by one interner per
//! isolate. Code that holds only the heap needs it as much as the shape
//! layer does: native leaves called from generated code, heap-only define
//! paths and the collector's root walk. So the interner lives in the heap's
//! isolate data ([`otter_gc::IsolateData`]), next to the canonical key
//! string of each atom and the integer-key atom cache, and every reader
//! reaches it through `&GcHeap` or `&mut GcHeap`.
//!
//! # Contents
//! - [`IsolateNames`] — the interner, the dense canonical key strings and
//!   the integer-key atom cache, the one weak shape interner and ephemeral
//!   primitive concatenations.
//! - [`names`] / [`names_mut`] — the interner of a heap's isolate.
//! - [`IndexAtomCacheLayout`] — the integer-key atom cache as generated
//!   keyed accesses read it, through `VmThread::index_atoms`.
//!
//! # Invariants
//! - One value per heap: the shape runtime installs it when it creates the
//!   heap's `null`-prototype root, or restores it from a snapshot's atom
//!   list, before any name is interned. Every reader may assume it.
//! - A canonical key string is an old-space flat string whose content is
//!   its atom's spelling and whose atom word records that atom. It lives as
//!   long as the isolate; the heap visits it as a root of every marking.
//! - `key_strings` is indexed by atom id; a null entry is an atom no shape
//!   has keyed yet. An entry, once set, never changes.
//! - The integer-key atom cache box never moves for the heap's life. A line
//!   whose key matches holds that key's permanent atom plus one, so
//!   generated code may trust a hit without any other check.
//! - Primitive concat memo entries are discarded before collection and
//!   never add roots.
//! - Shape tables hold only old-space handles; current-turn pins are strong,
//!   and all remaining shape entries are pruned before physical sweep.
//!
//! # See also
//! - [`crate::property_atom`] — atoms and the interner.
//! - [`crate::object::ShapeRuntime`] — mints canonical key strings for
//!   shape transitions.
//! - [`otter_gc::isolate_data`] — the heap's ownership contract.

use std::cell::Cell;

use otter_gc::GcHeap;
use otter_gc::raw::RawGc;

use crate::property_atom::{AtomId, NameInterner};
use crate::string::JsStringHandle;

/// Entries in the integer-key atom cache; a power of two.
const INDEX_ATOM_CACHE_ENTRIES: usize = 1024;

/// One integer-key cache line: the key, and its atom plus one (zero: no
/// string key spells it).
#[repr(C)]
#[derive(Clone, Copy)]
struct IndexAtomEntry {
    key: i32,
    atom_plus_one: u32,
}

/// Direct-mapped `int32 → atom` cache, indexed by `key & (entries - 1)`.
/// An empty line `i` holds the key `!i`, which maps to another line, so it
/// matches no probe.
#[repr(C)]
struct IndexAtomCache {
    entries: [IndexAtomEntry; INDEX_ATOM_CACHE_ENTRIES],
}

/// Where generated code finds an Int32 key's line in the integer-key atom
/// cache: line `key & mask`, `1 << entry_shift` bytes each, holding the key
/// at `key_byte` and its atom plus one (zero: none) at `atom_byte`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexAtomCacheLayout {
    /// Line-index mask; lines are a power of two.
    pub mask: u32,
    /// `log2` of the line size in bytes.
    pub entry_shift: u32,
    /// Byte offset of the line's `i32` key.
    pub key_byte: u32,
    /// Byte offset of the line's atom plus one.
    pub atom_byte: u32,
}

impl IndexAtomCacheLayout {
    /// The layout of this build.
    pub const CURRENT: Self = Self {
        mask: INDEX_ATOM_CACHE_ENTRIES as u32 - 1,
        entry_shift: std::mem::size_of::<IndexAtomEntry>().trailing_zeros(),
        key_byte: std::mem::offset_of!(IndexAtomEntry, key) as u32,
        atom_byte: std::mem::offset_of!(IndexAtomEntry, atom_plus_one) as u32,
    };
}

const _: () = assert!(std::mem::size_of::<IndexAtomEntry>().is_power_of_two());
const _: () = assert!(std::mem::offset_of!(IndexAtomCache, entries) == 0);

impl IndexAtomCache {
    fn new() -> Box<Self> {
        Box::new(Self {
            entries: std::array::from_fn(|line| IndexAtomEntry {
                key: !(line as i32),
                atom_plus_one: 0,
            }),
        })
    }
}

/// The isolate's property-name state; see the module docs.
pub(crate) struct IsolateNames {
    /// The isolate's one property-name interner.
    names: NameInterner,
    /// The heap's one canonical hidden-class and weak-lifetime authority.
    shapes: crate::object::shape_interner::ShapeInterner,
    /// The canonical key string of each atom, by atom id; null where no
    /// shape has keyed the atom.
    key_strings: Vec<Cell<JsStringHandle>>,
    index_atoms: Box<IndexAtomCache>,
    pub(crate) concat_cache: crate::string::concat_cache::ConcatCache,
}

impl otter_gc::IsolateData for IsolateNames {
    fn visit_old_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.shapes.trace_roots(visitor);
        for handle in &self.key_strings {
            if !handle.get().is_null() {
                visitor(handle.as_ptr().cast::<RawGc>());
            }
        }
    }
    fn prepare_collection(&self) {
        self.concat_cache.clear();
    }

    fn sweep_weak(&self, heap: &GcHeap) {
        self.shapes.sweep_dead(heap);
    }
}

impl IsolateNames {
    pub(crate) fn shapes(&self) -> &crate::object::shape_interner::ShapeInterner {
        &self.shapes
    }

    /// Give `heap` its isolate's names, starting from `names`.
    pub(crate) fn install(heap: &mut GcHeap, names: NameInterner) {
        heap.install_isolate_data(Self {
            names,
            shapes: Default::default(),
            key_strings: Vec::new(),
            index_atoms: IndexAtomCache::new(),
            concat_cache: crate::string::concat_cache::ConcatCache::empty(),
        });
    }

    /// `heap`'s isolate names, or `None` for a heap no shape runtime owns.
    #[inline]
    #[must_use]
    pub(crate) fn get(heap: &GcHeap) -> Option<&Self> {
        heap.isolate_data()
    }

    /// `heap`'s isolate names.
    ///
    /// # Panics
    /// When no shape runtime owns `heap`.
    #[inline]
    #[must_use]
    pub(crate) fn of(heap: &GcHeap) -> &Self {
        Self::get(heap).expect("the shape runtime installs the isolate's names")
    }

    /// [`Self::of`] for mutation.
    #[inline]
    pub(crate) fn of_mut(heap: &mut GcHeap) -> &mut Self {
        heap.isolate_data_mut()
            .expect("the shape runtime installs the isolate's names")
    }

    /// The isolate's property-name interner.
    #[inline]
    #[must_use]
    pub(crate) fn names(&self) -> &NameInterner {
        &self.names
    }

    /// The isolate's property-name interner, for minting atoms.
    #[inline]
    pub(crate) fn names_mut(&mut self) -> &mut NameInterner {
        &mut self.names
    }

    /// The atom spelling the canonical decimal of the non-negative `index`,
    /// when some name does: from the integer-key cache, else from the
    /// interner, caching the hit. An atom is permanent, so a cached line
    /// never goes stale; an unspelled index is not cached, since a later
    /// name may spell it.
    pub(crate) fn index_atom(&mut self, index: i32) -> Option<AtomId> {
        debug_assert!(index >= 0);
        let line = &mut self.index_atoms.entries[index as usize & (INDEX_ATOM_CACHE_ENTRIES - 1)];
        if line.key == index && line.atom_plus_one != 0 {
            return Some(AtomId::from_global(line.atom_plus_one - 1));
        }
        let atom = self.names.lookup(itoa::Buffer::new().format(index));
        if atom == AtomId::NONE {
            return None;
        }
        *line = IndexAtomEntry {
            key: index,
            atom_plus_one: atom.raw() + 1,
        };
        Some(atom)
    }

    /// Address of the integer-key atom cache, which never moves.
    #[must_use]
    pub(crate) fn index_atoms_addr(&self) -> *const u8 {
        std::ptr::from_ref::<IndexAtomCache>(&self.index_atoms).cast()
    }

    /// `atom`'s canonical key string, once a shape has keyed it.
    #[inline]
    #[must_use]
    pub(crate) fn key_string(&self, atom: AtomId) -> Option<JsStringHandle> {
        let handle = self.key_strings.get(atom.raw() as usize)?.get();
        (!handle.is_null()).then_some(handle)
    }

    /// Record `handle` as `atom`'s canonical key string.
    pub(crate) fn set_key_string(&mut self, atom: AtomId, handle: JsStringHandle) {
        let index = atom.raw() as usize;
        debug_assert!(
            self.key_string(atom).is_none(),
            "a canonical key string is set once"
        );
        if self.key_strings.len() <= index {
            self.key_strings
                .resize_with(index + 1, || Cell::new(JsStringHandle::null()));
        }
        self.key_strings[index].set(handle);
    }

    /// Number of atoms with a canonical key string.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn key_string_count(&self) -> usize {
        self.key_strings
            .iter()
            .filter(|handle| !handle.get().is_null())
            .count()
    }
}

/// The property-name interner of `heap`'s isolate.
#[inline]
#[must_use]
pub(crate) fn names(heap: &GcHeap) -> &NameInterner {
    IsolateNames::of(heap).names()
}

/// [`names`] for minting atoms.
#[inline]
pub(crate) fn names_mut(heap: &mut GcHeap) -> &mut NameInterner {
    IsolateNames::of_mut(heap).names_mut()
}
