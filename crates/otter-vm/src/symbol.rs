//! JavaScript `Symbol` primitive — identity-shared values with an
//! optional description.
//!
//! Two `Symbol(desc)` calls always produce **distinct** values even
//! when the description matches, modelled here by `Rc<SymbolBody>`
//! identity (`Rc::ptr_eq`). The shared global registry that backs
//! `Symbol.for(key)` / `Symbol.keyFor(sym)` lives on the
//! [`crate::Interpreter`] so symbols stay scoped to one VM instance.
//!
//! # Contents
//! - [`JsSymbol`] — heap handle wrapping `Rc<SymbolBody>`. `Clone`
//!   yields the same identity.
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
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-symbol-objects>
//! - <https://tc39.es/ecma262/#sec-well-known-symbols>
//! - <https://tc39.es/ecma262/#sec-symbol.for>
//! - <https://tc39.es/ecma262/#sec-symbol.keyfor>

use std::cell::RefCell;
use std::rc::Rc;

use crate::string::{JsString, StringError, StringHeap};

/// One `Symbol` body — the shared identity bearer. Cloning a
/// [`JsSymbol`] keeps the same `Rc`, so `ptr_eq` is the truth-bearer
/// for `===`.
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
    /// identity is still `Rc::ptr_eq` — but it lets the runtime
    /// fast-path well-known checks (e.g. `Symbol.iterator`,
    /// `Symbol.toPrimitive`) without walking a comparison table.
    pub well_known: Option<WellKnown>,
}

/// Heap handle for [`Value::Symbol`].
///
/// `Clone` shares `Rc<SymbolBody>` — strict-equal symbols are the
/// same handle. The struct is `Send`/`Sync`-free to match the rest
/// of the foundation's single-threaded runtime model.
#[derive(Debug, Clone)]
pub struct JsSymbol {
    body: Rc<SymbolBody>,
}

impl JsSymbol {
    /// Construct a fresh ordinary symbol with the given (optional)
    /// description. Two calls with the same description always
    /// produce distinct symbols, per ECMA-262 §20.4.1.1.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-symbol-description>
    #[must_use]
    pub fn new(description: Option<JsString>) -> Self {
        Self {
            body: Rc::new(SymbolBody {
                description,
                well_known: None,
            }),
        }
    }

    /// Construct a well-known symbol singleton. Used by
    /// [`WellKnownSymbols::new`] only — user code reaches these
    /// through `Symbol.<name>` static accessors.
    #[must_use]
    pub fn well_known(tag: WellKnown, description: JsString) -> Self {
        Self {
            body: Rc::new(SymbolBody {
                description: Some(description),
                well_known: Some(tag),
            }),
        }
    }

    /// Borrow the description, if any.
    #[must_use]
    pub fn description(&self) -> Option<&JsString> {
        self.body.description.as_ref()
    }

    /// Returns the well-known tag, if this symbol is one.
    #[must_use]
    pub fn well_known_tag(&self) -> Option<WellKnown> {
        self.body.well_known
    }

    /// Identity comparison — strict `===` for symbols.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.body, &other.body)
    }

    /// Raw `Rc`-data pointer for use as a hash / map key in the
    /// per-object symbol-property store. Anchor a [`JsSymbol`]
    /// handle for the lifetime of the pointer.
    #[must_use]
    pub fn identity_addr(&self) -> *const SymbolBody {
        Rc::as_ptr(&self.body)
    }

    /// Render the symbol per `Symbol.prototype.toString` —
    /// `Symbol(<desc>)` with empty description rendered as
    /// `Symbol()`. Spec §20.4.3.3.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-symboldescriptivestring>
    #[must_use]
    pub fn descriptive_string(&self) -> String {
        match &self.body.description {
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
    /// description text.
    ///
    /// # Errors
    /// Returns the first [`StringError`] encountered while interning
    /// description strings (only on heap-cap exhaustion).
    pub fn new(heap: &StringHeap) -> Result<Self, StringError> {
        let mut entries = Vec::with_capacity(WellKnown::all().len());
        for tag in WellKnown::all() {
            let desc = JsString::from_str(tag.description_text(), heap)?;
            entries.push(JsSymbol::well_known(*tag, desc));
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
    /// equal to `key`.
    pub fn for_key(&self, key: &str, heap: &StringHeap) -> Result<JsSymbol, StringError> {
        if let Some(sym) = self.lookup(key) {
            return Ok(sym);
        }
        let desc = JsString::from_str(key, heap)?;
        let sym = JsSymbol::new(Some(desc));
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_symbols_have_distinct_identity() {
        let a = JsSymbol::new(None);
        let b = JsSymbol::new(None);
        assert!(!a.ptr_eq(&b));
        let c = a.clone();
        assert!(a.ptr_eq(&c));
    }

    #[test]
    fn registry_dedupes_by_key() {
        let heap = StringHeap::default();
        let reg = SymbolRegistry::new();
        let a = reg.for_key("k", &heap).unwrap();
        let b = reg.for_key("k", &heap).unwrap();
        assert!(a.ptr_eq(&b));
        assert_eq!(reg.key_for(&a).as_deref(), Some("k"));
    }

    #[test]
    fn well_known_table_returns_stable_singletons() {
        let heap = StringHeap::default();
        let table = WellKnownSymbols::new(&heap).unwrap();
        let a = table.get(WellKnown::Iterator);
        let b = table.get(WellKnown::Iterator);
        assert!(a.ptr_eq(&b));
        assert_eq!(a.well_known_tag(), Some(WellKnown::Iterator));
        let other = table.get(WellKnown::ToPrimitive);
        assert!(!a.ptr_eq(&other));
    }

    #[test]
    fn descriptive_string_format() {
        let s = JsSymbol::new(Some(
            JsString::from_str("x", &StringHeap::default()).unwrap(),
        ));
        assert_eq!(s.descriptive_string(), "Symbol(x)");
        let none = JsSymbol::new(None);
        assert_eq!(none.descriptive_string(), "Symbol()");
    }

    #[test]
    fn key_for_returns_none_for_well_known() {
        let heap = StringHeap::default();
        let reg = SymbolRegistry::new();
        let table = WellKnownSymbols::new(&heap).unwrap();
        let iter = table.get(WellKnown::Iterator);
        assert!(reg.key_for(&iter).is_none());
    }
}
