//! The isolate's property-name state, owned and rooted by its heap.
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
//!   the integer-key atom cache.
//! - [`names`] / [`names_mut`] — the interner of a heap's isolate.
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
//! - The integer-key atom cache box never moves for the heap's life.
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
    /// The canonical key string of each atom, by atom id; null where no
    /// shape has keyed the atom.
    key_strings: Vec<Cell<JsStringHandle>>,
    #[expect(
        dead_code,
        reason = "read by integer keyed access through its stable address"
    )]
    index_atoms: Box<IndexAtomCache>,
}

impl otter_gc::IsolateData for IsolateNames {
    fn visit_old_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        for handle in &self.key_strings {
            if !handle.get().is_null() {
                visitor(handle.as_ptr().cast::<RawGc>());
            }
        }
    }
}

impl IsolateNames {
    /// Give `heap` its isolate's names, starting from `names`.
    pub(crate) fn install(heap: &mut GcHeap, names: NameInterner) {
        heap.install_isolate_data(Self {
            names,
            key_strings: Vec::new(),
            index_atoms: IndexAtomCache::new(),
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
