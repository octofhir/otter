//! Typed per-realm intrinsic slots.
//!
//! Boa-style typed registry: every well-known **prototype** that the
//! dispatch path looks up by name gets a dedicated slot. Bootstrap
//! runs once and caches the resolved handles; runtime lookups read
//! the slot directly instead of doing two `object::get()` calls
//! (global → ctor → prototype) on every call.
//!
//! # Contents
//! - [`RealmIntrinsics`] — typed slots for `%Object.prototype%`,
//!   `%Function.prototype%`, `%Array.prototype%`. Native-function-shaped
//!   constructors (`Promise`, `RegExp`, `Date`, `Iterator`, …) take
//!   a different resolution path; their prototypes are looked up
//!   through `NativeFunction::own_property_descriptor`.
//!
//! # Invariants
//! - Slots are populated by reading the `globalThis` graph **after**
//!   `BOOTSTRAP_ENTRIES` finishes running. Each slot is `None` until
//!   populate runs.
//! - The dispatch path treats `None` as a cache miss and falls back to
//!   the original string-lookup helper.
//! - Slots hold `JsObject` handles; tracing rides on the global object
//!   the bootstrap already roots.

use crate::object::{self, JsObject};

/// Resolved well-known prototype handles for one realm.
#[derive(Debug, Default, Clone)]
pub(crate) struct RealmIntrinsics {
    /// `%Object.prototype%`.
    pub object_prototype: Option<JsObject>,
    /// `%Function.prototype%`.
    pub function_prototype: Option<JsObject>,
    /// `%Array.prototype%`.
    pub array_prototype: Option<JsObject>,
}

impl RealmIntrinsics {
    /// Populate every slot by walking `global_this`. Called once at the
    /// end of `build_global_this_impl` after every `BuiltinIntrinsic`
    /// has run.
    pub(crate) fn populate(&mut self, heap: &mut otter_gc::GcHeap, global: JsObject) {
        let resolve_object_prototype = |ctor: JsObject| -> Option<JsObject> {
            object::get(ctor, heap, "prototype").and_then(|v| v.as_object())
        };

        if let Some(ctor) = object::get(global, heap, "Object").and_then(|v| v.as_object()) {
            self.object_prototype = resolve_object_prototype(ctor);
        }
        if let Some(ctor) = object::get(global, heap, "Function").and_then(|v| v.as_object()) {
            self.function_prototype = resolve_object_prototype(ctor);
        }
        // §23.1.3 — `Array.prototype` lives as own data property on
        // the constructor, regardless of whether the constructor is a
        // plain JsObject (legacy) or a NativeFunction (couch!).
        if let Some(value) = object::get(global, heap, "Array") {
            if let Some(ctor) = value.as_object() {
                self.array_prototype = resolve_object_prototype(ctor);
            } else if let Some(native) = value.as_native_function() {
                self.array_prototype = native
                    .own_property_descriptor(heap, "prototype")
                    .ok()
                    .flatten()
                    .and_then(|d| match d.kind {
                        crate::object::DescriptorKind::Data { value } => value.as_object(),
                        _ => None,
                    });
            }
        }
    }

    /// All slots empty?
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.object_prototype.is_none()
            && self.function_prototype.is_none()
            && self.array_prototype.is_none()
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
        assert!(slots.object_prototype.is_some(), "Object.prototype cached");
        assert!(
            slots.function_prototype.is_some(),
            "Function.prototype cached"
        );
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
