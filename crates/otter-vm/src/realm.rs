//! ECMAScript §9.3 Realm Records.
//!
//! A Realm holds the per-realm execution state — the intrinsic objects, the
//! global object, and (eventually) the global environment record. Multiple
//! realms can coexist within a single runtime so that cross-realm operations
//! such as `Reflect.construct(Error, [], otherRealm.Function)` resolve
//! prototypes against the correct realm via §10.2.3 GetFunctionRealm and
//! §10.1.14 GetPrototypeFromConstructor.
//!
//! Spec: <https://tc39.es/ecma262/#sec-code-realms>

use crate::intrinsics::VmIntrinsics;

/// Stable identifier of a [`Realm`] inside [`crate::interpreter::RuntimeState`].
///
/// Realm IDs are densely allocated (zero-based) and never reused — a realm
/// can be created but is never freed for the lifetime of the runtime, so
/// closures and bound functions can keep their `[[Realm]]` slot as a plain
/// `RealmId` without GC tracking.
pub type RealmId = u32;

/// A single ECMAScript Realm Record (§9.3).
///
/// At present this is a thin wrapper around [`VmIntrinsics`]. Subsequent
/// refactor passes will hoist `global_object` and `global_env` here, and add
/// per-realm template maps for tagged template caching.
pub struct Realm {
    /// §9.3 \[\[Intrinsics\]\] — the realm-scoped intrinsic objects (all
    /// constructors, prototypes, and well-known objects accessible to script
    /// evaluated in this realm).
    pub intrinsics: VmIntrinsics,
}

impl Realm {
    /// Constructs a new realm wrapping the given intrinsic registry.
    pub fn new(intrinsics: VmIntrinsics) -> Self {
        Self { intrinsics }
    }
}
