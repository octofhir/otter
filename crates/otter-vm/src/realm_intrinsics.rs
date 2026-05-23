//! Typed per-realm intrinsic slots.
//!
//! Boa-style typed registry: every well-known constructor + prototype
//! that the dispatch path looks up by name gets a dedicated slot.
//! Bootstrap runs once and caches the resolved handles; runtime
//! lookups read the slot directly instead of doing two
//! `object::get()` calls (global → ctor → prototype) on every call.
//!
//! # Contents
//! - [`RealmIntrinsics`] — typed slots for `%Object%`,
//!   `%Object.prototype%`, `%Function.prototype%`, `%Array%`,
//!   `%Array.prototype%`. Native-function-shaped constructors
//!   (`Promise`, `RegExp`, `Date`, `Iterator`, …) are intentionally
//!   excluded for now — they take a different resolution path; a
//!   follow-up can promote them to `NativeFunction`-typed slots once
//!   the hot-path payoff justifies the per-slot polymorphism.
//! - Populate hook called once at the end of `build_global_this_impl`.
//!
//! # Invariants
//! - Slots are populated by reading the `globalThis` graph **after**
//!   `BOOTSTRAP_ENTRIES` finishes running. Each slot is `None` until
//!   populate runs.
//! - The dispatch path treats `None` as a cache miss and falls back to
//!   the original string-lookup helper. The lookup helpers still exist
//!   so any embedder that builds a non-default global (e.g. partial
//!   feature gates) keeps working.
//! - Slots hold `JsObject` handles; tracing rides on the global object
//!   the bootstrap already roots.

use crate::object::{self, JsObject};
use crate::value::Value;

/// Resolved well-known intrinsic handles for one realm.
#[derive(Debug, Default, Clone)]
pub(crate) struct RealmIntrinsics {
    /// `%Object%` constructor.
    pub object_constructor: Option<JsObject>,
    /// `%Object.prototype%`.
    pub object_prototype: Option<JsObject>,
    /// `%Function.prototype%`.
    pub function_prototype: Option<JsObject>,
    /// `%Array%` constructor.
    pub array_constructor: Option<JsObject>,
    /// `%Array.prototype%`.
    pub array_prototype: Option<JsObject>,
}

impl RealmIntrinsics {
    /// Populate every slot by walking `global_this`. Called once at the
    /// end of `build_global_this_impl` after every `BuiltinIntrinsic`
    /// has run. Each lookup is a single `global.get(name)` + at most
    /// one `ctor.get("prototype")`; the post-bootstrap cost is fixed,
    /// not per-call.
    pub(crate) fn populate(&mut self, heap: &otter_gc::GcHeap, global: JsObject) {
        let resolve_ctor = |name: &'static str| -> Option<JsObject> {
            object::get(global, heap, name).and_then(|v| v.as_object())
        };
        let resolve_proto = |ctor: JsObject| -> Option<JsObject> {
            object::get(ctor, heap, "prototype").and_then(|v| v.as_object())
        };

        if let Some(ctor) = resolve_ctor("Object") {
            self.object_constructor = Some(ctor);
            self.object_prototype = resolve_proto(ctor);
        }
        if let Some(ctor) = resolve_ctor("Function") {
            self.function_prototype = resolve_proto(ctor);
        }
        if let Some(ctor) = resolve_ctor("Array") {
            self.array_constructor = Some(ctor);
            self.array_prototype = resolve_proto(ctor);
        }
    }

    /// All slots empty?
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.object_constructor.is_none()
            && self.object_prototype.is_none()
            && self.function_prototype.is_none()
            && self.array_constructor.is_none()
            && self.array_prototype.is_none()
    }

    /// `%Object%` constructor as a [`Value`].
    #[allow(dead_code)]
    pub(crate) fn object_constructor_value(&self) -> Option<Value> {
        self.object_constructor.map(Value::object)
    }

    /// `%Array%` constructor's `prototype` as a [`Value`].
    #[allow(dead_code)]
    pub(crate) fn array_prototype_value(&self) -> Option<Value> {
        self.array_prototype.map(Value::object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Interpreter;

    #[test]
    fn bootstrap_populates_well_known_slots() {
        let interp = Interpreter::new();
        let slots = &interp.realm_intrinsics();
        // The plain-JsObject-shaped intrinsics are the ones the
        // dispatch path looks up on every call; assert each lands
        // in the typed slot. Native-function-shaped constructors
        // (Promise, RegExp, …) are wired through a different
        // resolution path and intentionally skipped here.
        assert!(slots.object_constructor.is_some(), "Object cached");
        assert!(slots.object_prototype.is_some(), "Object.prototype cached");
        assert!(
            slots.function_prototype.is_some(),
            "Function.prototype cached"
        );
        assert!(slots.array_constructor.is_some(), "Array cached");
        assert!(slots.array_prototype.is_some(), "Array.prototype cached");
    }

    #[test]
    fn slot_matches_string_lookup_for_object_prototype() {
        let interp = Interpreter::new();
        let slot_proto = interp.realm_intrinsics().object_prototype.unwrap();
        let global = *interp.global_this();
        let ctor = crate::object::get(global, interp.gc_heap(), "Object")
            .and_then(|v| v.as_object())
            .unwrap();
        let walked = crate::object::get(ctor, interp.gc_heap(), "prototype")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(
            slot_proto, walked,
            "RealmIntrinsics slot must point at the same %Object.prototype% \
             that the global-walk resolves"
        );
    }

    #[test]
    fn slot_matches_string_lookup_for_function_prototype() {
        let interp = Interpreter::new();
        let slot_proto = interp.realm_intrinsics().function_prototype.unwrap();
        let global = *interp.global_this();
        let ctor = crate::object::get(global, interp.gc_heap(), "Function")
            .and_then(|v| v.as_object())
            .unwrap();
        let walked = crate::object::get(ctor, interp.gc_heap(), "prototype")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(
            slot_proto, walked,
            "RealmIntrinsics slot must point at the same %Function.prototype% \
             that the global-walk resolves"
        );
    }

    #[test]
    fn default_is_empty() {
        let slots = RealmIntrinsics::default();
        assert!(slots.is_empty(), "default RealmIntrinsics has no slots");
    }
}
