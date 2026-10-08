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
//! - The interner is keyed by a hash of UTF-16 code units, so a runtime
//!   string's atom is found from its Latin-1 or wide units in place. Its one
//!   owner is the isolate's shape runtime; no lock guards it.
//! - Each interned spelling has one backing allocation shared by the name-to-id
//!   map and the id-to-name table.
//! - A chunk's atom table starts unresolved and is resolved by the interpreter
//!   that links or adopts it. Resolution is idempotent, so a code space adopted
//!   by a different interpreter re-keys to that interpreter's interner instead
//!   of carrying foreign ids into its shapes.
//! - The atom text is borrowed from a frozen [`AtomTable`], which lives as long
//!   as its linked chunk.
//! - Computed property keys and symbols do not pass through this layer.
//! - Decoded atom arrays and spellings are prepared fallibly after the owning
//!   payload admits their physical geometry; allocator refusal publishes no IDs.
//!
//! # See also
//! - [`crate::execution_context`]
//! - [`crate::property_dispatch`]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use otter_bytecode::Constant;

/// Isolate-global atom id for a string property key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
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
/// Like V8's string table, the table is keyed by a hash of the UTF-16 code
/// units, so a runtime string looks its atom up from its own Latin-1 or wide
/// units with no decoded copy. The isolate's single mutator owns it through
/// its shape runtime: no lock guards any read or mint.
#[derive(Debug, Default)]
pub(crate) struct NameInterner {
    /// Atom ids by content hash; equality reads the id's spelling.
    table: hashbrown::HashTable<u32>,
    /// Id → spelling. Append-only: index equals the atom's id.
    names: Vec<Arc<str>>,
    /// Id → content hash, so growing the table rehashes no spelling.
    hashes: Vec<u64>,
}

/// One code-unit view of a candidate spelling.
#[derive(Clone, Copy)]
pub(crate) enum NameUnits<'a> {
    Latin1(&'a [u8]),
    Utf16(&'a [u16]),
}

const NAME_HASH_SEED: u64 = 0x9e37_79b9_7f4a_7c15;
const NAME_HASH_MULTIPLIER: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// Hash of a sequence of UTF-16 code units: equal for every representation
/// of the same content.
fn hash_units(units: impl Iterator<Item = u16>) -> u64 {
    let mut hash = NAME_HASH_SEED;
    for unit in units {
        hash = (hash.rotate_left(5) ^ u64::from(unit)).wrapping_mul(NAME_HASH_MULTIPLIER);
    }
    hash
}

impl NameUnits<'_> {
    fn hash(self) -> u64 {
        match self {
            Self::Latin1(bytes) => hash_units(bytes.iter().map(|&byte| u16::from(byte))),
            Self::Utf16(units) => hash_units(units.iter().copied()),
        }
    }

    /// `true` when `spelling` has exactly these code units. A lone surrogate
    /// matches no spelling: a Rust string encodes none.
    fn spells(self, spelling: &str) -> bool {
        match self {
            Self::Latin1(bytes) if spelling.is_ascii() => spelling.as_bytes() == bytes,
            Self::Latin1(bytes) => spelling
                .encode_utf16()
                .eq(bytes.iter().map(|&byte| u16::from(byte))),
            Self::Utf16(units) => spelling.encode_utf16().eq(units.iter().copied()),
        }
    }
}

impl NameInterner {
    fn hash_str(name: &str) -> u64 {
        if name.is_ascii() {
            NameUnits::Latin1(name.as_bytes()).hash()
        } else {
            hash_units(name.encode_utf16())
        }
    }

    /// Return `name`'s isolate-global atom id, minting one on first sight.
    #[must_use]
    pub(crate) fn intern(&mut self, name: &str) -> AtomId {
        let hash = Self::hash_str(name);
        let names = &self.names;
        if let Some(&id) = self.table.find(hash, |&id| *names[id as usize] == *name) {
            return AtomId(id);
        }
        let id = u32::try_from(self.names.len()).expect("isolate exhausted the atom id range");
        assert_ne!(
            id,
            AtomId::UNRESOLVED,
            "isolate exhausted the atom id range"
        );
        self.names.push(Arc::from(name));
        self.hashes.push(hash);
        let hashes = &self.hashes;
        self.table
            .insert_unique(hash, id, |&id| hashes[id as usize]);
        AtomId(id)
    }

    /// The spelling `atom` was minted for.
    #[must_use]
    pub(crate) fn spelling(&self, atom: AtomId) -> Option<&Arc<str>> {
        self.names.get(atom.raw() as usize)
    }

    /// `name`'s atom id when some shape, chunk or cache has interned it, or
    /// [`AtomId::NONE`] for a spelling never interned. Nothing is minted:
    /// every shape key is interned when its transition is built, so a
    /// never-interned spelling names no shaped property, and a lookup keyed
    /// by [`AtomId::NONE`] still finds dictionary-mode properties by spelling.
    #[must_use]
    pub(crate) fn lookup(&self, name: &str) -> AtomId {
        let names = &self.names;
        self.table
            .find(Self::hash_str(name), |&id| *names[id as usize] == *name)
            .map_or(AtomId::NONE, |&id| AtomId(id))
    }

    /// [`Self::lookup`] of a string's code units, read in place.
    #[must_use]
    pub(crate) fn lookup_units(&self, units: NameUnits<'_>) -> AtomId {
        let names = &self.names;
        self.table
            .find(units.hash(), |&id| units.spells(&names[id as usize]))
            .map_or(AtomId::NONE, |&id| AtomId(id))
    }

    /// The whole table in id order, for the isolate snapshot: index
    /// equals the atom's id, so replaying the list through [`Self::intern`]
    /// on a fresh isolate re-mints identical ids.
    #[must_use]
    pub(crate) fn snapshot_names(&self) -> Vec<Box<str>> {
        self.names
            .iter()
            .map(|name| Box::<str>::from(name.as_ref()))
            .collect()
    }

    /// Number of distinct property names this isolate has interned.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.names.len()
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
/// The owning chunk admits array/spelling geometry before this builder
/// prepares buffers fallibly. Runtime dispatch receives only the frozen table;
/// no atom identity is published by preparation.
#[derive(Debug, Default)]
pub(crate) struct AtomTableBuilder {
    slots: Vec<AtomSlot>,
}

impl AtomTableBuilder {
    /// Build atom metadata from a bytecode constant pool.
    #[must_use]
    pub(crate) fn from_constants(
        constants: &[Constant],
        lease: &mut otter_resource::ResourceLease,
    ) -> Result<Self, otter_resource::ResourceError> {
        let mut builder = Self {
            slots: crate::executable::allocation::try_vec(constants.len(), lease)?,
        };
        for constant in constants {
            builder.push(constant, lease)?;
        }
        Ok(builder)
    }

    fn push(
        &mut self,
        constant: &Constant,
        lease: &mut otter_resource::ResourceLease,
    ) -> Result<(), otter_resource::ResourceError> {
        let text = match constant {
            Constant::String { utf16 } => {
                Some(crate::executable::allocation::try_utf16(utf16, lease)?)
            }
            _ => None,
        };
        self.slots.push(AtomSlot {
            text,
            id: AtomicU32::new(AtomId::UNRESOLVED),
        });
        Ok(())
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
    pub(crate) fn from_constants(
        constants: &[Constant],
        lease: &mut otter_resource::ResourceLease,
    ) -> Result<Self, otter_resource::ResourceError> {
        Ok(AtomTableBuilder::from_constants(constants, lease)?.freeze())
    }

    /// Requested physical metadata geometry before any decoding allocation.
    pub(crate) fn allocation_bytes(constants: &[Constant]) -> u64 {
        constants.iter().fold(
            crate::executable::allocation::array_bytes::<AtomSlot>(constants.len()),
            |bytes, constant| {
                bytes.saturating_add(match constant {
                    Constant::String { utf16 } => crate::executable::allocation::utf16_bytes(utf16),
                    _ => 0,
                })
            },
        )
    }

    /// Heap bytes this table retains for the owning chunk's lifetime: the
    /// slot array plus every decoded string spelling.
    #[must_use]
    pub(crate) fn retained_bytes(&self) -> u64 {
        let mut total = std::mem::size_of_val::<[AtomSlot]>(&self.slots) as u64;
        for slot in &self.slots {
            if let Some(text) = &slot.text {
                total = total.saturating_add(text.capacity() as u64);
            }
        }
        total
    }

    /// Resolve every string constant to `names`' global atom id.
    ///
    /// Idempotent: resolving twice against the same interner rewrites the same
    /// ids, and resolving against a different interner re-keys the whole table.
    /// That is what makes adopting a foreign code space sound — the adopting
    /// interpreter's ids are the only ones its shapes ever see.
    pub(crate) fn resolve(&self, names: &mut NameInterner) {
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

    fn atom_builder(constants: &[Constant]) -> AtomTableBuilder {
        let mut lease = otter_resource::ResourceAccount::default()
            .reserve_exact(
                otter_resource::ResourceClass::SourceModuleBytes,
                AtomTable::allocation_bytes(constants),
            )
            .expect("admit atom fixture metadata");
        AtomTableBuilder::from_constants(constants, &mut lease).expect("prepare atom fixture")
    }

    fn atom_table(constants: &[Constant]) -> AtomTable {
        atom_builder(constants).freeze()
    }

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

        let mut names = NameInterner::default();
        let table = atom_builder(&constants).freeze();
        table.resolve(&mut names);

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
        let mut names = NameInterner::default();
        let first = atom_table(&[
            Constant::String { utf16: utf16("y") },
            Constant::String { utf16: utf16("x") },
        ]);
        let second = atom_table(&[Constant::String { utf16: utf16("x") }]);
        first.resolve(&mut names);
        second.resolve(&mut names);

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
        let mut names = NameInterner::default();
        let table = atom_table(&[
            Constant::String { utf16: utf16("x") },
            Constant::String { utf16: utf16("x") },
        ]);
        table.resolve(&mut names);

        assert_eq!(
            table.property_atom(0).unwrap().atom().id(),
            table.property_atom(1).unwrap().atom().id(),
        );
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn resolution_rekeys_to_the_adopting_interner() {
        let table = atom_table(&[Constant::String { utf16: utf16("x") }]);
        let mut first = NameInterner::default();
        let mut second = NameInterner::default();
        // Give the second interner a different id for the same spelling.
        let _ = second.intern("unrelated");

        table.resolve(&mut first);
        let before = table.property_atom(0).unwrap().atom().id();
        table.resolve(&mut second);
        let after = table.property_atom(0).unwrap().atom().id();

        assert_eq!(before, first.intern("x"));
        assert_eq!(after, second.intern("x"));
        assert_ne!(before, after);
    }

    // The check is a debug-build invariant of the dispatch hot path.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "no interpreter has linked or adopted")]
    fn unresolved_table_refuses_to_hand_out_an_atom() {
        let table = atom_table(&[Constant::String { utf16: utf16("x") }]);
        let _ = table.property_atom(0);
    }

    #[test]
    fn interner_reverse_lookup_names_its_atoms() {
        let mut names = NameInterner::default();
        let atom = names.intern("length");
        assert_eq!(names.spelling(atom).map(|name| &**name), Some("length"));
        assert_eq!(names.spelling(AtomId::from_global(99)), None);
    }

    #[test]
    fn code_unit_lookup_matches_every_representation() {
        let mut names = NameInterner::default();
        let ascii = names.intern("length");
        let latin1 = names.intern("caf\u{e9}");
        let wide = names.intern("\u{3b1}\u{1f600}");
        assert_eq!(names.lookup_units(NameUnits::Latin1(b"length")), ascii);
        assert_eq!(
            names.lookup_units(NameUnits::Utf16(&utf16("length"))),
            ascii
        );
        assert_eq!(names.lookup_units(NameUnits::Latin1(b"caf\xe9")), latin1);
        assert_eq!(
            names.lookup_units(NameUnits::Utf16(&utf16("caf\u{e9}"))),
            latin1
        );
        assert_eq!(
            names.lookup_units(NameUnits::Utf16(&utf16("\u{3b1}\u{1f600}"))),
            wide
        );
        assert_eq!(names.lookup("\u{3b1}\u{1f600}"), wide);
        assert_eq!(
            names.lookup_units(NameUnits::Latin1(b"lengths")),
            AtomId::NONE
        );
        // A lone surrogate spells nothing a Rust string can.
        assert_eq!(
            names.lookup_units(NameUnits::Utf16(&[0xd83d])),
            AtomId::NONE
        );
        assert_eq!(names.len(), 3);
    }
}
