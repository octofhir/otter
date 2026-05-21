//! JavaScript `Symbol` primitive — identity-shared values with an
//! optional description.
//!
//! Two `Symbol(desc)` calls always produce **distinct** values even
//! when the description matches; identity is the per-symbol GC body
//! ([`SymbolBody`]) reachable through a [`SymbolHandle`]. The shared
//! global registry that backs `Symbol.for(key)` / `Symbol.keyFor(sym)`
//! lives on the [`crate::Interpreter`] so symbols stay scoped to one
//! VM instance.
//!
//! # Contents
//! - [`SymbolBody`] — GC-allocated identity bearer + canonical
//!   description / well-known tag / registry flag.
//! - [`JsSymbol`] — heap handle wrapping [`SymbolHandle`]
//!   (`Gc<SymbolBody>`) plus a cached copy of the per-symbol
//!   descriptor fields so reads stay heap-free.
//! - [`WellKnown`] — enum of every well-known symbol named by the
//!   spec (§6.1.5.1). Each tag maps to a stable singleton `JsSymbol`
//!   in the per-interpreter [`WellKnownSymbols`] table.
//! - [`WellKnownSymbols`] — eager-init lookup table populated by
//!   [`Interpreter::new`]. Stable across the interpreter's lifetime.
//! - [`SymbolRegistry`] — registry that `Symbol.for` / `keyFor`
//!   walk. Lives on the [`Interpreter`].
//!
//! # Invariants
//! - `JsSymbol::ptr_eq` is the only correctness-bearing equality —
//!   description text is informational, never compared for identity.
//! - Well-known symbols are NOT placed in the [`SymbolRegistry`];
//!   `Symbol.keyFor(Symbol.iterator)` returns `undefined` per spec.
//! - The registry's keys are arbitrary user strings ("foo", "@k",
//!   …); descriptions on registered symbols are forced to match the
//!   key per §20.4.2.4 step 9.
//! - Every live `JsSymbol` reachable from a root (registry, well-known
//!   table, value-model slot) must be visited by
//!   [`crate::gc_trace::GcTrace`] so the embedded `SymbolHandle`
//!   stays live across collections.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-symbol-objects>
//! - <https://tc39.es/ecma262/#sec-well-known-symbols>
//! - <https://tc39.es/ecma262/#sec-symbol.for>
//! - <https://tc39.es/ecma262/#sec-symbol.keyfor>

use std::cell::RefCell;

use otter_gc::raw::SlotVisitor;

use crate::string::JsString;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`SymbolBody`].
pub const SYMBOL_BODY_TYPE_TAG: u8 = 0x26;

/// 4-byte compressed GC handle to a [`SymbolBody`]. `Copy`. Packs
/// into [`crate::value::Value`] under `TAG_PTR_OTHER`.
pub type SymbolHandle = otter_gc::Gc<SymbolBody>;

/// Allocate a `SymbolBody` on the GC heap.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_symbol(
    heap: &mut otter_gc::GcHeap,
    description: Option<JsString>,
    well_known: Option<WellKnown>,
    registered: bool,
) -> Result<SymbolHandle, otter_gc::OutOfMemory> {
    heap.alloc_old(SymbolBody {
        description,
        well_known,
        registered,
    })
}

/// One `Symbol` body — the GC-allocated identity bearer plus the
/// per-symbol descriptor fields. Allocation is always through
/// [`alloc_symbol`]; cloning a [`JsSymbol`] copies the handle but
/// keeps the same body, so `ptr_eq` is the truth-bearer for `===`.
#[derive(Debug)]
pub struct SymbolBody {
    /// Optional human-readable description, exposed through
    /// [`Symbol.prototype.description`] and the `toString` form
    /// `Symbol(<desc>)`. `None` means "no description" — distinct
    /// from a description that is the empty string.
    pub description: Option<JsString>,
    /// Stable tag identifying which well-known slot this symbol
    /// belongs to. `None` for ordinary `Symbol(...)` calls and
    /// registry entries; `Some` for the singletons populated by
    /// [`WellKnownSymbols::new`]. The tag is informational —
    /// identity is still handle equality — but it lets the runtime
    /// fast-path well-known checks (e.g. `Symbol.iterator`,
    /// `Symbol.toPrimitive`) without walking a comparison table.
    pub well_known: Option<WellKnown>,
    /// `true` for symbols created by `Symbol.for`. Registered
    /// symbols are shared through the global registry and cannot be
    /// used as WeakMap / WeakSet keys.
    pub registered: bool,
}

impl otter_gc::SafeTraceable for SymbolBody {
    const TYPE_TAG: u8 = SYMBOL_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, _visitor: &mut SlotVisitor<'_>) {
        // `description` is `JsString` — an `Arc<StringRepr>` outside
        // the cage today; no GC slot to follow. Once `JsString`
        // migrates to a GC body the description handle slot is
        // emitted here.
    }
}

/// Heap handle for [`Value::Symbol`].
///
/// Backed by a GC body ([`SymbolBody`]) reached through a 4-byte
/// compressed [`SymbolHandle`]. The wrapper carries cached copies of
/// the descriptor fields so `description()`, `well_known_tag()`,
/// `is_registered()`, and `descriptive_string()` stay heap-free —
/// matches the JsIntl / JsTemporal cache pattern in
/// `docs/value-cutover-plan.md`. Cloning copies the cache and the
/// handle (a 4-byte offset), preserving identity.
#[derive(Debug, Clone)]
pub struct JsSymbol {
    inner: SymbolHandle,
    description: Option<JsString>,
    well_known: Option<WellKnown>,
    registered: bool,
}

impl JsSymbol {
    /// Construct a fresh ordinary symbol with the given (optional)
    /// description. Two calls with the same description always
    /// produce distinct symbols, per ECMA-262 §20.4.1.1.
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim from the body
    /// allocation.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-symbol-description>
    pub fn new(
        heap: &mut otter_gc::GcHeap,
        description: Option<JsString>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let inner = alloc_symbol(heap, description.clone(), None, false)?;
        Ok(Self {
            inner,
            description,
            well_known: None,
            registered: false,
        })
    }

    /// Construct a well-known symbol singleton. Used by
    /// [`WellKnownSymbols::new`] only — user code reaches these
    /// through `Symbol.<name>` static accessors.
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    pub fn well_known(
        heap: &mut otter_gc::GcHeap,
        tag: WellKnown,
        description: JsString,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let inner = alloc_symbol(heap, Some(description.clone()), Some(tag), false)?;
        Ok(Self {
            inner,
            description: Some(description),
            well_known: Some(tag),
            registered: false,
        })
    }

    /// Construct a registered symbol — `Symbol.for` step 4. The
    /// description is forced to match the registry key per §20.4.2.4
    /// step 9.
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    pub fn registered(
        heap: &mut otter_gc::GcHeap,
        description: JsString,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let inner = alloc_symbol(heap, Some(description.clone()), None, true)?;
        Ok(Self {
            inner,
            description: Some(description),
            well_known: None,
            registered: true,
        })
    }

    /// Whether this symbol came from `Symbol.for`.
    #[must_use]
    pub fn is_registered(&self) -> bool {
        self.registered
    }

    /// Borrow the description, if any. Reads the wrapper-side cache;
    /// no heap touch.
    #[must_use]
    pub fn description(&self) -> Option<&JsString> {
        self.description.as_ref()
    }

    /// Returns the well-known tag, if this symbol is one. Reads the
    /// wrapper-side cache.
    #[must_use]
    pub fn well_known_tag(&self) -> Option<WellKnown> {
        self.well_known
    }

    /// Identity comparison — strict `===` for symbols. Follows
    /// compressed-offset equality on the GC handle.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Stable identity token for hashing the symbol as a map key.
    /// Returns the underlying GC handle's compressed offset cast to
    /// `usize`; two clones of the same symbol return the same value.
    #[must_use]
    pub fn identity_addr(&self) -> usize {
        self.inner.offset() as usize
    }

    /// Raw GC handle — used by tracing and write barriers.
    #[doc(hidden)]
    #[inline]
    #[must_use]
    pub fn handle(&self) -> SymbolHandle {
        self.inner
    }

    /// Visit the embedded GC handle so the scavenger can rewrite the
    /// compressed offset in place if the body moves. Called from
    /// [`crate::Value::trace_value_slots`] and from
    /// [`crate::gc_trace::GcTrace`] adapters for the registry / table.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = &self.inner as *const SymbolHandle as *mut otter_gc::raw::RawGc;
        visitor(p);
    }

    /// Render the symbol per `Symbol.prototype.toString` —
    /// `Symbol(<desc>)` with empty description rendered as
    /// `Symbol()`. Spec §20.4.3.3.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-symboldescriptivestring>
    #[must_use]
    pub fn descriptive_string(&self) -> String {
        match &self.description {
            Some(s) => format!("Symbol({})", s.to_lossy_string()),
            None => "Symbol()".to_string(),
        }
    }
}

/// Tag enumerating every well-known symbol the spec defines
/// (ECMA-262 §6.1.5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WellKnown {
    /// `@@asyncIterator` — iterator factory for `for await … of`.
    /// Spec §27.1.2.1.
    AsyncIterator,
    /// `@@hasInstance` — `instanceof` override. Spec §22.2.1.2.
    HasInstance,
    /// `@@isConcatSpreadable` — `Array.prototype.concat` spread
    /// switch. Spec §23.1.1.5.
    IsConcatSpreadable,
    /// `@@iterator` — iterator factory for `for…of`, spread,
    /// destructuring. Spec §22.1.1.
    Iterator,
    /// `@@match` — `String.prototype.match` regex hook. Spec §22.1.3.
    Match,
    /// `@@matchAll` — `String.prototype.matchAll`. Spec §22.1.4.
    MatchAll,
    /// `@@replace` — `String.prototype.replace`. Spec §22.1.5.
    Replace,
    /// `@@search` — `String.prototype.search`. Spec §22.1.6.
    Search,
    /// `@@species` — subclass-aware constructor. Spec §22.1.7.
    Species,
    /// `@@split` — `String.prototype.split`. Spec §22.1.8.
    Split,
    /// `@@toPrimitive` — `ToPrimitive` hook. Spec §22.1.9.
    ToPrimitive,
    /// `@@toStringTag` — `Object.prototype.toString` tag.
    /// Spec §22.1.10.
    ToStringTag,
    /// `@@unscopables` — `with`-statement exclusion list.
    /// Spec §22.1.11. The runtime does not implement `with`, but the
    /// well-known symbol must still exist for spec conformance.
    Unscopables,
}

impl WellKnown {
    /// JS-visible name on the `Symbol` namespace (e.g. `"iterator"`
    /// for `Symbol.iterator`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            WellKnown::AsyncIterator => "asyncIterator",
            WellKnown::HasInstance => "hasInstance",
            WellKnown::IsConcatSpreadable => "isConcatSpreadable",
            WellKnown::Iterator => "iterator",
            WellKnown::Match => "match",
            WellKnown::MatchAll => "matchAll",
            WellKnown::Replace => "replace",
            WellKnown::Search => "search",
            WellKnown::Species => "species",
            WellKnown::Split => "split",
            WellKnown::ToPrimitive => "toPrimitive",
            WellKnown::ToStringTag => "toStringTag",
            WellKnown::Unscopables => "unscopables",
        }
    }

    /// Description string used by `Symbol.<name>.description`
    /// (e.g. `"Symbol.iterator"`). Spec §6.1.5.1 mandates this
    /// exact wording.
    #[must_use]
    pub const fn description_text(self) -> &'static str {
        match self {
            WellKnown::AsyncIterator => "Symbol.asyncIterator",
            WellKnown::HasInstance => "Symbol.hasInstance",
            WellKnown::IsConcatSpreadable => "Symbol.isConcatSpreadable",
            WellKnown::Iterator => "Symbol.iterator",
            WellKnown::Match => "Symbol.match",
            WellKnown::MatchAll => "Symbol.matchAll",
            WellKnown::Replace => "Symbol.replace",
            WellKnown::Search => "Symbol.search",
            WellKnown::Species => "Symbol.species",
            WellKnown::Split => "Symbol.split",
            WellKnown::ToPrimitive => "Symbol.toPrimitive",
            WellKnown::ToStringTag => "Symbol.toStringTag",
            WellKnown::Unscopables => "Symbol.unscopables",
        }
    }

    /// Resolve a `Symbol.<name>` member name to its tag.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "asyncIterator" => WellKnown::AsyncIterator,
            "hasInstance" => WellKnown::HasInstance,
            "isConcatSpreadable" => WellKnown::IsConcatSpreadable,
            "iterator" => WellKnown::Iterator,
            "match" => WellKnown::Match,
            "matchAll" => WellKnown::MatchAll,
            "replace" => WellKnown::Replace,
            "search" => WellKnown::Search,
            "species" => WellKnown::Species,
            "split" => WellKnown::Split,
            "toPrimitive" => WellKnown::ToPrimitive,
            "toStringTag" => WellKnown::ToStringTag,
            "unscopables" => WellKnown::Unscopables,
            _ => return None,
        })
    }

    /// Every well-known tag in declaration order. Used by the
    /// [`WellKnownSymbols::new`] table builder.
    #[must_use]
    pub const fn all() -> &'static [WellKnown] {
        &[
            WellKnown::AsyncIterator,
            WellKnown::HasInstance,
            WellKnown::IsConcatSpreadable,
            WellKnown::Iterator,
            WellKnown::Match,
            WellKnown::MatchAll,
            WellKnown::Replace,
            WellKnown::Search,
            WellKnown::Species,
            WellKnown::Split,
            WellKnown::ToPrimitive,
            WellKnown::ToStringTag,
            WellKnown::Unscopables,
        ]
    }
}

/// Per-interpreter table of well-known symbol singletons. Eagerly
/// initialised during [`crate::Interpreter::new`] so any reader path
/// can hand out the canonical `Rc` without an allocation.
#[derive(Debug)]
pub struct WellKnownSymbols {
    entries: Vec<JsSymbol>,
}

impl WellKnownSymbols {
    /// Allocate every well-known symbol with its spec-mandated
    /// description text. Each entry's [`SymbolBody`] lives on the GC
    /// heap; root tracing keeps them alive across collections.
    ///
    /// # Errors
    /// Returns the first [`WellKnownInitError`] encountered while
    /// interning a description string or allocating a body.
    pub fn new(gc_heap: &mut otter_gc::GcHeap) -> Result<Self, WellKnownInitError> {
        let mut entries = Vec::with_capacity(WellKnown::all().len());
        for tag in WellKnown::all() {
            let desc = JsString::from_str(tag.description_text(), gc_heap)?;
            entries.push(JsSymbol::well_known(gc_heap, *tag, desc)?);
        }
        Ok(Self { entries })
    }

    /// Resolve a tag to its singleton symbol.
    #[must_use]
    pub fn get(&self, tag: WellKnown) -> JsSymbol {
        // Linear scan over 13 entries; no observable cost.
        self.entries
            .iter()
            .find(|s| s.well_known_tag() == Some(tag))
            .cloned()
            .expect("well-known table populated by new()")
    }

    /// Iterate the singletons for root tracing.
    pub(crate) fn entries(&self) -> impl Iterator<Item = &JsSymbol> {
        self.entries.iter()
    }
}

/// Init-time failure for [`WellKnownSymbols::new`]. Folds
/// [`otter_gc::OutOfMemory`] sources from description interning and
/// GC body allocation so the per-realm bootstrap can surface either
/// with one error type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WellKnownInitError {
    /// GC body allocation failed.
    #[error(transparent)]
    OutOfMemory(#[from] otter_gc::OutOfMemory),
}

/// Global symbol registry backing `Symbol.for(key)` /
/// `Symbol.keyFor(sym)`. ECMA-262 §20.4.2.4 / §20.4.2.6.
///
/// Foundation choice: a flat `Vec<(String, JsSymbol)>`. The registry
/// is rarely used (the spec calls it the *GlobalSymbolRegistry* and
/// real engines back it with a hashmap, but conformance fixtures
/// touch it sparsely). The vector keeps insertion order so
/// `keyFor` returns the original key as-is.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-globalsymbolregistry>
#[derive(Debug, Default)]
pub struct SymbolRegistry {
    entries: RefCell<Vec<(String, JsSymbol)>>,
}

impl SymbolRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Spec §20.4.2.4 `Symbol.for(key)`: return the registered symbol
    /// for `key`, or create + register a new one with description
    /// equal to `key`. The freshly allocated symbol's body lives on
    /// the GC heap; root tracing visits registry entries so the body
    /// stays live as long as the registry retains it.
    ///
    /// # Errors
    /// Surfaces [`otter_gc::OutOfMemory`] from description interning
    /// and body allocation.
    pub fn for_key(
        &self,
        gc_heap: &mut otter_gc::GcHeap,
        key: &str,
    ) -> Result<JsSymbol, SymbolRegistryError> {
        if let Some(sym) = self.lookup(key) {
            return Ok(sym);
        }
        let desc = JsString::from_str(key, gc_heap)?;
        let sym = JsSymbol::registered(gc_heap, desc)?;
        self.entries
            .borrow_mut()
            .push((key.to_string(), sym.clone()));
        Ok(sym)
    }

    /// Spec §20.4.2.6 `Symbol.keyFor(sym)`: return the registry key
    /// for the given symbol, or `None` if `sym` is not registered.
    /// Identity comparison via [`JsSymbol::ptr_eq`].
    #[must_use]
    pub fn key_for(&self, sym: &JsSymbol) -> Option<String> {
        self.entries
            .borrow()
            .iter()
            .find(|(_, registered)| registered.ptr_eq(sym))
            .map(|(k, _)| k.clone())
    }

    /// Internal lookup by key. Returns the registered symbol if any.
    fn lookup(&self, key: &str) -> Option<JsSymbol> {
        self.entries
            .borrow()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, s)| s.clone())
    }

    /// Run `f` against each registered symbol. Used by
    /// [`crate::gc_trace::GcTrace`] to keep registry entries live
    /// across collections.
    pub(crate) fn for_each_entry(&self, mut f: impl FnMut(&JsSymbol)) {
        for (_, sym) in self.entries.borrow().iter() {
            f(sym);
        }
    }
}

/// Failure modes returned by [`SymbolRegistry::for_key`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SymbolRegistryError {
    /// GC body allocation failed (description interning or body).
    #[error(transparent)]
    OutOfMemory(#[from] otter_gc::OutOfMemory),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_gc_heap() -> otter_gc::GcHeap {
        otter_gc::GcHeap::new().expect("gc heap")
    }

    #[test]
    fn fresh_symbols_have_distinct_identity() {
        let mut gc = fresh_gc_heap();
        let a = JsSymbol::new(&mut gc, None).unwrap();
        let b = JsSymbol::new(&mut gc, None).unwrap();
        assert!(!a.ptr_eq(&b));
        let c = a.clone();
        assert!(a.ptr_eq(&c));
    }

    #[test]
    fn registry_dedupes_by_key() {
        let mut gc = fresh_gc_heap();
        let reg = SymbolRegistry::new();
        let a = reg.for_key(&mut gc, "k").unwrap();
        let b = reg.for_key(&mut gc, "k").unwrap();
        assert!(a.ptr_eq(&b));
        assert_eq!(reg.key_for(&a).as_deref(), Some("k"));
    }

    #[test]
    fn well_known_table_returns_stable_singletons() {
        let mut gc = fresh_gc_heap();
        let table = WellKnownSymbols::new(&mut gc).unwrap();
        let a = table.get(WellKnown::Iterator);
        let b = table.get(WellKnown::Iterator);
        assert!(a.ptr_eq(&b));
        assert_eq!(a.well_known_tag(), Some(WellKnown::Iterator));
        let other = table.get(WellKnown::ToPrimitive);
        assert!(!a.ptr_eq(&other));
    }

    #[test]
    fn descriptive_string_format() {
        let mut gc = fresh_gc_heap();
        let desc = JsString::from_str("x", &gc).unwrap();
        let s = JsSymbol::new(&mut gc, Some(desc)).unwrap();
        assert_eq!(s.descriptive_string(), "Symbol(x)");
        let none = JsSymbol::new(&mut gc, None).unwrap();
        assert_eq!(none.descriptive_string(), "Symbol()");
    }

    #[test]
    fn key_for_returns_none_for_well_known() {
        let mut gc = fresh_gc_heap();
        let reg = SymbolRegistry::new();
        let table = WellKnownSymbols::new(&mut gc).unwrap();
        let iter = table.get(WellKnown::Iterator);
        assert!(reg.key_for(&iter).is_none());
    }
}
