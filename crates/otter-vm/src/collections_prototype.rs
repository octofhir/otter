//! Collection prototype method-name metadata.
//!
//! Executable `Map` / `Set` / `WeakMap` / `WeakSet` prototype
//! methods are installed by [`crate::bootstrap_collections`] through
//! the `couch!` native surface; property reads on collections walk their
//! live prototype chain like any other receiver. This module keeps only the
//! direct-call guard predicates.
//!
//! # Contents
//! - `is_*_builtin_method` — `CallMethodValue` guard predicates.
//!
//! # Invariants
//! - JS-visible methods come from the bootstrap/native surface.
//! - `WeakMap` / `WeakSet` deliberately expose no `size` accessor.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-map-prototype-object>
//! - <https://tc39.es/ecma262/#sec-set-prototype-object>
//! - <https://tc39.es/ecma262/#sec-weakmap-prototype-object>
//! - <https://tc39.es/ecma262/#sec-weakset-prototype-object>

/// Whether `name` is installed on `Map.prototype`.
#[must_use]
pub fn is_map_builtin_method(name: &str) -> bool {
    matches!(
        name,
        "get" | "set" | "has" | "delete" | "clear" | "keys" | "values" | "entries" | "forEach"
    )
}

/// Whether `name` is installed on `Set.prototype`.
#[must_use]
pub fn is_set_builtin_method(name: &str) -> bool {
    matches!(
        name,
        "add"
            | "has"
            | "delete"
            | "clear"
            | "keys"
            | "values"
            | "entries"
            | "forEach"
            | "union"
            | "intersection"
            | "difference"
            | "symmetricDifference"
            | "isSubsetOf"
            | "isSupersetOf"
            | "isDisjointFrom"
    )
}

/// Whether `name` is installed on `WeakMap.prototype`.
#[must_use]
pub fn is_weak_map_builtin_method(name: &str) -> bool {
    matches!(name, "get" | "set" | "has" | "delete")
}

/// Whether `name` is installed on `WeakSet.prototype`.
#[must_use]
pub fn is_weak_set_builtin_method(name: &str) -> bool {
    matches!(name, "add" | "has" | "delete")
}
