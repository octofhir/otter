//! Watchpoint system — invalidate compiled code when runtime invariants break.
//!
//! Inspired by JSC's watchpoint system: JIT code can depend on invariants
//! (prototype chain unchanged, global not modified, shape not transitioned)
//! without inserting runtime checks. When an invariant breaks, all dependent
//! compiled code is invalidated.
//!
//! ## State machine
//!
//! ```text
//! Clear → Watched → Invalidated (permanent)
//! ```
//!
//! - **Clear**: No one is watching. Mutations are free.
//! - **Watched**: JIT code depends on this invariant. Mutations must fire.
//! - **Invalidated**: Invariant was broken. Cannot be re-watched.
//!   All dependent code must be discarded.
//!
//! JSC reference: Three states, adaptive watchpoints relocate to new structures.
//! SM reference: On-Stack Invalidation (OSI) patches call sites.
//!
//! Spec: Phase 5.3 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};

// ============================================================
// Watchpoint States
// ============================================================

/// State of a watchpoint set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WatchpointState {
    /// No one is watching. Mutations are free.
    Clear = 0,
    /// JIT code depends on this invariant. Mutations trigger invalidation.
    Watched = 1,
    /// Invariant was broken. Permanent — cannot be re-watched.
    Invalidated = 2,
}

impl WatchpointState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Clear,
            1 => Self::Watched,
            _ => Self::Invalidated,
        }
    }
}

// ============================================================
// WatchpointSet
// ============================================================

/// A single watchpoint set guarding one invariant.
///
/// Thread-safe via atomic state (single-threaded VM, but safe for future).
#[derive(Debug)]
pub struct WatchpointSet {
    state: AtomicU8,
}

impl WatchpointSet {
    /// Create a new watchpoint set in the Clear state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(WatchpointState::Clear as u8),
        }
    }

    /// Get the current state.
    #[must_use]
    pub fn state(&self) -> WatchpointState {
        WatchpointState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Transition from Clear to Watched.
    ///
    /// Called when JIT code is compiled that depends on this invariant.
    /// Returns false if already Invalidated (too late to watch).
    pub fn watch(&self) -> bool {
        match self.state() {
            WatchpointState::Clear => {
                self.state
                    .store(WatchpointState::Watched as u8, Ordering::Release);
                true
            }
            WatchpointState::Watched => true, // Already watched.
            WatchpointState::Invalidated => false, // Too late.
        }
    }

    /// Fire the watchpoint: transition to Invalidated.
    ///
    /// Called when the invariant is violated (e.g., prototype chain mutated).
    /// Returns true if the watchpoint was Watched (i.e., code needs invalidation).
    pub fn fire(&self) -> bool {
        let prev = self.state.swap(WatchpointState::Invalidated as u8, Ordering::AcqRel);
        prev == WatchpointState::Watched as u8
    }

    /// Whether the invariant is still valid (not invalidated).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.state() != WatchpointState::Invalidated
    }

    /// Whether JIT code is depending on this invariant.
    #[must_use]
    pub fn is_watched(&self) -> bool {
        self.state() == WatchpointState::Watched
    }
}

impl Default for WatchpointSet {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================
// Watchpoint Kinds
// ============================================================

/// What invariant a watchpoint guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchpointKind {
    /// Prototype chain of an object hasn't changed.
    /// Key: object handle (u32).
    PrototypeChain(u32),
    /// A global variable hasn't been reassigned.
    /// Key: property name hash.
    GlobalConstancy(u32),
    /// An object's shape hasn't transitioned.
    /// Key: shape ID.
    ShapeStability(u64),
    /// A function hasn't been redefined.
    /// Key: function index.
    FunctionIdentity(u32),
}

// ============================================================
// WatchpointRegistry — manages all watchpoints
// ============================================================

/// Registry of all active watchpoints in the runtime.
///
/// Maps watchpoint kinds to their sets, and tracks which compiled
/// functions depend on which watchpoints.
#[derive(Debug)]
pub struct WatchpointRegistry {
    /// Watchpoint sets keyed by kind.
    sets: HashMap<WatchpointKind, WatchpointSet>,
    /// Dependencies: watchpoint kind → set of dependent function keys.
    /// When a watchpoint fires, all dependent functions must be invalidated.
    dependents: HashMap<WatchpointKind, Vec<DependentCode>>,
}

/// A piece of compiled code that depends on a watchpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DependentCode {
    /// Identifier for the compiled function (e.g., function pointer address or cache key).
    pub function_key: u64,
    /// Human-readable name for diagnostics.
    pub function_name: String,
}

impl WatchpointRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sets: HashMap::new(),
            dependents: HashMap::new(),
        }
    }

    /// Get or create a watchpoint set for a given kind.
    pub fn get_or_create(&mut self, kind: WatchpointKind) -> &WatchpointSet {
        self.sets.entry(kind).or_insert_with(WatchpointSet::new)
    }

    /// Watch an invariant and register a dependent compiled function.
    ///
    /// Returns false if the invariant is already invalidated.
    pub fn watch(&mut self, kind: WatchpointKind, dependent: DependentCode) -> bool {
        let set = self.sets.entry(kind).or_insert_with(WatchpointSet::new);
        if !set.watch() {
            return false; // Already invalidated.
        }
        self.dependents
            .entry(kind)
            .or_default()
            .push(dependent);
        true
    }

    /// Fire a watchpoint and return all dependent functions that need invalidation.
    ///
    /// Called when a runtime mutation violates an invariant.
    /// Creates the set in Invalidated state if it doesn't exist yet,
    /// preventing future watchers from depending on a broken invariant.
    pub fn fire(&mut self, kind: WatchpointKind) -> Vec<DependentCode> {
        let set = self.sets.entry(kind).or_insert_with(WatchpointSet::new);
        let was_watched = set.fire();

        if was_watched {
            self.dependents.remove(&kind).unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Check if a watchpoint is still valid.
    #[must_use]
    pub fn is_valid(&self, kind: &WatchpointKind) -> bool {
        self.sets
            .get(kind)
            .map(|s| s.is_valid())
            .unwrap_or(true) // No set = never invalidated.
    }

    /// Number of active watchpoint sets.
    #[must_use]
    pub fn set_count(&self) -> usize {
        self.sets.len()
    }

    /// Number of total dependents across all watchpoints.
    #[must_use]
    pub fn total_dependents(&self) -> usize {
        self.dependents.values().map(|v| v.len()).sum()
    }
}

impl Default for WatchpointRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watchpoint_lifecycle() {
        let wp = WatchpointSet::new();
        assert_eq!(wp.state(), WatchpointState::Clear);

        assert!(wp.watch());
        assert_eq!(wp.state(), WatchpointState::Watched);

        // Fire → Invalidated.
        assert!(wp.fire()); // Returns true because it was Watched.
        assert_eq!(wp.state(), WatchpointState::Invalidated);

        // Cannot re-watch.
        assert!(!wp.watch());
        assert_eq!(wp.state(), WatchpointState::Invalidated);
    }

    #[test]
    fn test_fire_unwatched() {
        let wp = WatchpointSet::new();
        // Fire while Clear → no code to invalidate.
        assert!(!wp.fire());
        assert_eq!(wp.state(), WatchpointState::Invalidated);
    }

    #[test]
    fn test_registry_watch_and_fire() {
        let mut reg = WatchpointRegistry::new();
        let kind = WatchpointKind::PrototypeChain(1);
        let dep = DependentCode {
            function_key: 42,
            function_name: "hot_fn".into(),
        };

        assert!(reg.watch(kind, dep.clone()));
        assert!(reg.is_valid(&kind));
        assert_eq!(reg.total_dependents(), 1);

        // Fire → get dependents back.
        let invalids = reg.fire(kind);
        assert_eq!(invalids.len(), 1);
        assert_eq!(invalids[0].function_key, 42);
        assert!(!reg.is_valid(&kind));
    }

    #[test]
    fn test_registry_multiple_dependents() {
        let mut reg = WatchpointRegistry::new();
        let kind = WatchpointKind::GlobalConstancy(100);

        reg.watch(kind, DependentCode { function_key: 1, function_name: "f1".into() });
        reg.watch(kind, DependentCode { function_key: 2, function_name: "f2".into() });
        reg.watch(kind, DependentCode { function_key: 3, function_name: "f3".into() });

        let invalids = reg.fire(kind);
        assert_eq!(invalids.len(), 3);
    }

    #[test]
    fn test_registry_fire_unregistered() {
        let mut reg = WatchpointRegistry::new();
        let kind = WatchpointKind::ShapeStability(999);

        // Firing an unregistered watchpoint: no dependents.
        let invalids = reg.fire(kind);
        assert!(invalids.is_empty());
    }

    #[test]
    fn test_watch_after_invalidation_fails() {
        let mut reg = WatchpointRegistry::new();
        let kind = WatchpointKind::FunctionIdentity(5);

        // Fire without watching first.
        reg.fire(kind);

        // Now try to watch — should fail.
        let dep = DependentCode { function_key: 10, function_name: "late".into() };
        assert!(!reg.watch(kind, dep));
    }
}
