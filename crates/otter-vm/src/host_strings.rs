//! Opaque host-side property atoms.
//!
//! Host bindings often probe the same property names on every call. This
//! module interns those names into clone-cheap, process-portable handles while
//! keeping bytecode atom tables and VM representations private.
//!
//! # Contents
//! - [`HostAtomInterner`] — thread-safe host name interner.
//! - [`HostAtom`] / [`HostAtomId`] — stable opaque name handles and ids.
//!
//! # Invariants
//! - Atom ids never expose a VM constant-pool or GC representation.
//! - Interners and atoms are owned, `Send + Sync`, and allocation-free to clone.
//! - JavaScript access still occurs only through a mutator-bound native scope.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

static NEXT_INTERNER_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_ATOM_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct HostAtomEntry {
    interner: u64,
    id: u64,
    text: Box<str>,
}

/// Stable opaque identifier for a host atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostAtomId {
    interner: u64,
    id: u64,
}

/// Interned host property name.
#[derive(Clone)]
pub struct HostAtom(Arc<HostAtomEntry>);

impl HostAtom {
    /// Return the opaque stable identifier for this atom.
    #[must_use]
    pub fn id(&self) -> HostAtomId {
        HostAtomId {
            interner: self.0.interner,
            id: self.0.id,
        }
    }

    /// Borrow the logical property spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0.text
    }
}

impl std::fmt::Debug for HostAtom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("HostAtom").field(&self.id()).finish()
    }
}

impl PartialEq for HostAtom {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

impl Eq for HostAtom {}

impl Hash for HostAtom {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id().hash(state);
    }
}

#[derive(Debug)]
struct HostAtomInternerInner {
    id: u64,
    entries: RwLock<HashMap<String, Arc<HostAtomEntry>>>,
}

/// Thread-safe interner for property names used by host bindings.
///
/// The table owns each spelling for the interner's lifetime, so re-interning a
/// name always returns the same id even after earlier [`HostAtom`] handles are
/// dropped.
#[derive(Debug, Clone)]
pub struct HostAtomInterner(Arc<HostAtomInternerInner>);

impl HostAtomInterner {
    /// Create an empty interner.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(HostAtomInternerInner {
            id: NEXT_INTERNER_ID.fetch_add(1, Ordering::Relaxed),
            entries: RwLock::new(HashMap::new()),
        }))
    }

    /// Intern one property spelling.
    #[must_use]
    pub fn intern(&self, name: &str) -> HostAtom {
        if let Some(entry) = self
            .0
            .entries
            .read()
            .expect("host atom interner poisoned")
            .get(name)
            .cloned()
        {
            return HostAtom(entry);
        }

        let mut entries = self.0.entries.write().expect("host atom interner poisoned");
        if let Some(entry) = entries.get(name).cloned() {
            return HostAtom(entry);
        }
        let entry = Arc::new(HostAtomEntry {
            interner: self.0.id,
            id: NEXT_ATOM_ID.fetch_add(1, Ordering::Relaxed),
            text: name.into(),
        });
        entries.insert(name.to_string(), entry.clone());
        HostAtom(entry)
    }
}

impl Default for HostAtomInterner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_interning_reuses_stable_identity() {
        let interner = HostAtomInterner::new();
        let first = interner.intern("onclick");
        let id = first.id();
        let second = interner.intern("onclick");
        assert_eq!(first, second);
        assert_eq!(first.id(), second.id());
        assert_eq!(first.as_str(), "onclick");
        drop(first);
        drop(second);
        assert_eq!(interner.intern("onclick").id(), id);
    }

    #[test]
    fn public_atoms_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HostAtom>();
        assert_send_sync::<HostAtomId>();
        assert_send_sync::<HostAtomInterner>();
    }
}
