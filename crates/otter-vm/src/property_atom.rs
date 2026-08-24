//! Isolate-global property-name atoms for executable VM code.
//!
//! A property name has exactly one identity per isolate: the index it was
//! assigned by that isolate's [`NameInterner`]. Every real engine does this
//! (V8 internalized strings, SpiderMonkey atoms, JSC `UniquedStringImpl`)
//! because it turns every name comparison on a hot path — shape-chain walks,
//! IC guards, transition replay — into a `u32` compare instead of a string
//! content compare.
//!
//! The bytecode constant pool remains the public/debug source of truth. Each
//! linked chunk carries an [`AtomTable`] that maps its string constant indexes
//! to the owning isolate's global atom ids, resolved once when the chunk is
//! linked into an interpreter.
//!
//! # Contents
//! - [`AtomId`] — isolate-global string-key id.
//! - [`NameInterner`] — append-only name → id map owned by one isolate.
//! - [`PropertyAtom`] — compact identity for one property name.
//! - [`AtomizedPropertyKey`] — borrowed runtime view: atom id plus text.
//! - [`AtomTableBuilder`] and [`AtomTable`] — transient build step and frozen
//!   execution table for decoded strings and named-property atoms.
//!
//! # Invariants
//! - Atom ids are global within one isolate: two chunks that spell the same
//!   property name resolve to the same id, and different names never share one.
//! - The interner is append-only and never evicts. Ids are permanent for the
//!   isolate's life, which is what lets shapes and caches store bare ids.
//! - Each interned spelling has one backing allocation shared by the name-to-id
//!   map and the id-to-name table.
//! - A chunk's atom table starts unresolved and is resolved by the interpreter
//!   that links or adopts it. Resolution is idempotent, so a code space adopted
//!   by a different interpreter re-keys to that interpreter's interner instead
//!   of carrying foreign ids into its shapes.
//! - The atom text is borrowed from a frozen [`AtomTable`], which lives as long
//!   as its linked chunk.
//! - Computed property keys and symbols do not pass through this layer.
//!
//! # See also
//! - [`crate::execution_context`]
//! - [`crate::property_dispatch`]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use otter_bytecode::Constant;
use rustc_hash::FxHashMap;

/// Isolate-global atom id for a string property key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct AtomId(u32);

impl AtomId {
    /// Reserved id an unresolved atom-table slot holds. Never handed out by
    /// [`NameInterner::intern`].
    const UNRESOLVED: u32 = u32::MAX;

    /// The atom of a shape node that adds no key — the hidden-class root.
    /// Shares the reserved id, so it equals no interned name and a chain walk
    /// needs no separate root test.
    pub(crate) const NONE: Self = Self(Self::UNRESOLVED);

    /// Wrap a global id minted by an isolate's [`NameInterner`].
    #[must_use]
    pub(crate) const fn from_global(id: u32) -> Self {
        Self(id)
    }

    /// Raw global id, for keying isolate-owned side tables.
    #[must_use]
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }
}

/// Append-only property-name interner owned by one isolate.
///
/// Interning happens off the hot path: at chunk link time for bytecode string
/// constants, and on a first-ever shape transition for a runtime-built name.
/// Every hot comparison then reads a pre-resolved [`AtomId`].
///
/// The lock exists because a [`crate::code_space::CodeSpace`] is `Sync` and
/// several linkers may resolve chunks against one interner; it is never taken
/// by property dispatch.
#[derive(Debug, Default)]
pub(crate) struct NameInterner {
    inner: Mutex<InternedNames>,
}

#[derive(Debug, Default)]
struct InternedNames {
    ids: FxHashMap<Arc<str>, u32>,
    /// Id → name, for diagnostics. Append-only: index equals the atom's id.
    names: Vec<Arc<str>>,
}

impl NameInterner {
    /// Return `name`'s isolate-global atom id, minting one on first sight.
    #[must_use]
    pub(crate) fn intern(&self, name: &str) -> AtomId {
        let mut inner = self.inner.lock().expect("name interner");
        if let Some(id) = inner.ids.get(name) {
            return AtomId(*id);
        }
        let id = u32::try_from(inner.names.len()).expect("isolate exhausted the atom id range");
        assert_ne!(
            id,
            AtomId::UNRESOLVED,
            "isolate exhausted the atom id range"
        );
        let spelling: Arc<str> = Arc::from(name);
        inner.names.push(Arc::clone(&spelling));
        inner.ids.insert(spelling, id);
        AtomId(id)
    }

    /// The whole table in id order, for the isolate snapshot: index
    /// equals the atom's id, so replaying the list through [`Self::intern`]
    /// on a fresh isolate re-mints identical ids.
    #[must_use]
    pub(crate) fn snapshot_names(&self) -> Vec<Box<str>> {
        self.inner
            .lock()
            .expect("name interner")
            .names
            .iter()
            .map(|name| Box::<str>::from(name.as_ref()))
            .collect()
    }

    /// Spelling of an interned atom, for diagnostics. Copies out because the
    /// storage is behind the interner's lock.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn name(&self, atom: AtomId) -> Option<Box<str>> {
        let inner = self.inner.lock().expect("name interner");
        inner
            .names
            .get(atom.raw() as usize)
            .map(|name| Box::<str>::from(name.as_ref()))
    }

    /// Number of distinct property names this isolate has interned.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().expect("name interner").names.len()
    }
}

/// Compact identity for one property name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PropertyAtom {
    id: AtomId,
}

impl PropertyAtom {
    /// Create an atom identity.
    #[must_use]
    pub(crate) const fn new(id: AtomId) -> Self {
        Self { id }
    }

    /// Isolate-global id.
    #[must_use]
    pub(crate) const fn id(self) -> AtomId {
        self.id
    }
}

/// Borrowed view of an atomized property key for one dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AtomizedPropertyKey<'a> {
    atom: PropertyAtom,
    text: &'a str,
}

impl<'a> AtomizedPropertyKey<'a> {
    /// Create a borrowed atomized key view.
    #[must_use]
    pub(crate) const fn new(atom: PropertyAtom, text: &'a str) -> Self {
        Self { atom, text }
    }

    /// Isolate-global atom identity.
    #[must_use]
    pub(crate) const fn atom(self) -> PropertyAtom {
        self.atom
    }

    /// String spelling used for dictionary-mode and non-atomized lookups.
    #[must_use]
    pub(crate) const fn name(self) -> &'a str {
        self.text
    }
}

/// One constant-pool slot as dispatch reads it: the decoded spelling of a
/// string constant and its global atom id, side by side so resolving a named
/// property touches one entry rather than two parallel arrays.
#[derive(Debug)]
struct AtomSlot {
    /// Decoded UTF-8 spelling, `None` for non-string constants.
    text: Option<String>,
    /// Global atom id, [`AtomId::UNRESOLVED`] until an interpreter resolves
    /// the chunk against its interner. Non-string slots keep the sentinel.
    id: AtomicU32,
}

/// Transient builder for [`AtomTable`].
///
/// The builder may allocate and decode freely while an
/// [`crate::ExecutionContext`] is being constructed. Runtime dispatch receives
/// only the frozen table.
#[derive(Debug, Default)]
pub(crate) struct AtomTableBuilder {
    slots: Vec<AtomSlot>,
}

impl AtomTableBuilder {
    /// Build atom metadata from a bytecode constant pool.
    #[must_use]
    pub(crate) fn from_constants(constants: &[Constant]) -> Self {
        let mut builder = Self {
            slots: Vec::with_capacity(constants.len()),
        };
        for constant in constants {
            builder.push(constant);
        }
        builder
    }

    fn push(&mut self, constant: &Constant) {
        let text = match constant {
            Constant::String { utf16 } => Some(String::from_utf16_lossy(utf16)),
            _ => None,
        };
        self.slots.push(AtomSlot {
            text,
            id: AtomicU32::new(AtomId::UNRESOLVED),
        });
    }

    /// Seal the transient buffers into an immutable execution table whose atom
    /// ids are still unresolved.
    #[must_use]
    pub(crate) fn freeze(self) -> AtomTable {
        AtomTable {
            slots: self.slots.into_boxed_slice(),
        }
    }
}

/// Frozen atom table published with an [`crate::ExecutionContext`].
#[derive(Debug)]
pub(crate) struct AtomTable {
    slots: Box<[AtomSlot]>,
}

impl AtomTable {
    /// Build and freeze an unresolved atom table from a bytecode constant pool.
    #[must_use]
    pub(crate) fn from_constants(constants: &[Constant]) -> Self {
        AtomTableBuilder::from_constants(constants).freeze()
    }

    /// Resolve every string constant to `names`' global atom id.
    ///
    /// Idempotent: resolving twice against the same interner rewrites the same
    /// ids, and resolving against a different interner re-keys the whole table.
    /// That is what makes adopting a foreign code space sound — the adopting
    /// interpreter's ids are the only ones its shapes ever see.
    pub(crate) fn resolve(&self, names: &NameInterner) {
        for slot in &self.slots {
            if let Some(text) = slot.text.as_deref() {
                slot.id.store(names.intern(text).raw(), Ordering::Relaxed);
            }
        }
    }

    /// Resolve a string constant as a borrowed UTF-8 string.
    #[must_use]
    pub(crate) fn string_constant_str(&self, idx: u32) -> Option<&str> {
        self.slots.get(idx as usize)?.text.as_deref()
    }

    /// Resolve a string constant as an atomized property key.
    #[must_use]
    pub(crate) fn property_atom(&self, idx: u32) -> Option<AtomizedPropertyKey<'_>> {
        let slot = self.slots.get(idx as usize)?;
        let text = slot.text.as_deref()?;
        let id = slot.id.load(Ordering::Relaxed);
        // Every named-property dispatch passes here, so the check that the
        // chunk was linked or adopted is a debug-build invariant. In release
        // an unresolved id simply matches no shape and no cache entry, which
        // degrades to the slow path rather than answering wrongly.
        debug_assert_ne!(
            id,
            AtomId::UNRESOLVED,
            "named-property dispatch reached an atom table no interpreter has linked or adopted",
        );
        Some(AtomizedPropertyKey::new(
            PropertyAtom::new(AtomId::from_global(id)),
            text,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static_assertions::assert_impl_all!(NameInterner: Send, Sync);

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn builder_freezes_decoded_strings_and_atoms() {
        let constants = vec![
            Constant::Number {
                bits: 1.0f64.to_bits(),
            },
            Constant::String {
                utf16: utf16("name"),
            },
            Constant::String {
                utf16: utf16("other"),
            },
        ];

        let names = NameInterner::default();
        let table = AtomTableBuilder::from_constants(&constants).freeze();
        table.resolve(&names);

        assert_eq!(table.string_constant_str(0), None);
        assert_eq!(table.string_constant_str(1), Some("name"));
        assert_eq!(table.string_constant_str(2), Some("other"));

        let key = table.property_atom(1).expect("string atom");
        assert_eq!(key.name(), "name");
        assert_eq!(key.atom().id(), names.intern("name"));
        assert!(table.property_atom(0).is_none());
    }

    #[test]
    fn one_name_has_one_id_across_chunks() {
        let names = NameInterner::default();
        let first = AtomTable::from_constants(&[
            Constant::String { utf16: utf16("y") },
            Constant::String { utf16: utf16("x") },
        ]);
        let second = AtomTable::from_constants(&[Constant::String { utf16: utf16("x") }]);
        first.resolve(&names);
        second.resolve(&names);

        assert_eq!(
            first.property_atom(1).unwrap().atom().id(),
            second.property_atom(0).unwrap().atom().id(),
            "the same spelling in two chunks is one atom",
        );
        assert_ne!(
            first.property_atom(0).unwrap().atom().id(),
            first.property_atom(1).unwrap().atom().id(),
        );
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn duplicate_constants_share_one_atom() {
        let names = NameInterner::default();
        let table = AtomTable::from_constants(&[
            Constant::String { utf16: utf16("x") },
            Constant::String { utf16: utf16("x") },
        ]);
        table.resolve(&names);

        assert_eq!(
            table.property_atom(0).unwrap().atom().id(),
            table.property_atom(1).unwrap().atom().id(),
        );
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn resolution_rekeys_to_the_adopting_interner() {
        let table = AtomTable::from_constants(&[Constant::String { utf16: utf16("x") }]);
        let first = NameInterner::default();
        let second = NameInterner::default();
        // Give the second interner a different id for the same spelling.
        let _ = second.intern("unrelated");

        table.resolve(&first);
        let before = table.property_atom(0).unwrap().atom().id();
        table.resolve(&second);
        let after = table.property_atom(0).unwrap().atom().id();

        assert_eq!(before, first.intern("x"));
        assert_eq!(after, second.intern("x"));
        assert_ne!(before, after);
    }

    #[test]
    #[should_panic(expected = "no interpreter has linked or adopted")]
    fn unresolved_table_refuses_to_hand_out_an_atom() {
        let table = AtomTable::from_constants(&[Constant::String { utf16: utf16("x") }]);
        let _ = table.property_atom(0);
    }

    #[test]
    fn interner_reverse_lookup_names_its_atoms() {
        let names = NameInterner::default();
        let atom = names.intern("length");
        assert_eq!(names.name(atom).as_deref(), Some("length"));
        assert_eq!(names.name(AtomId::from_global(99)), None);
    }

    #[test]
    fn interner_indexes_share_one_spelling_allocation() {
        let names = NameInterner::default();
        let atom = names.intern("length");
        let inner = names.inner.lock().expect("name interner");
        let indexed = &inner.names[atom.raw() as usize];
        let (mapped, _) = inner.ids.get_key_value("length").expect("interned name");

        assert!(Arc::ptr_eq(indexed, mapped));
        assert_eq!(Arc::strong_count(indexed), 2);
    }
}
