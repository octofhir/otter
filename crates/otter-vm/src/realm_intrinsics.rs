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
//!   `%Function.prototype%`, `%Array.prototype%`, and
//!   `%Promise.prototype%`. Native-function-shaped constructors that
//!   are not on hot object-dispatch paths still resolve through
//!   `NativeFunction::own_property_descriptor`.
//!
//! # Invariants
//! - Slots are populated by reading the `globalThis` graph **after**
//!   `BOOTSTRAP_ENTRIES` finishes running. Each slot is `None` until
//!   populate runs.
//! - The dispatch path treats `None` as a cache miss and falls back to
//!   the original string-lookup helper.
//! - Slots hold `JsObject` handles and are traced as runtime roots so
//!   moving GC rewrites the cached handles in place.

use crate::gc_trace::{GcRootVisitor, GcTrace};
use crate::object::{self, JsObject};

/// Look up `<name>.prototype` on `globalThis`, accepting either a
/// plain JsObject constructor or a `NativeFunction` constructor.
/// `couch!`-emitted constructors are NativeFunctions; legacy
/// installers (currently only Function) still emit plain JsObjects.
fn resolve_prototype(
    global: JsObject,
    heap: &mut otter_gc::GcHeap,
    ctor_name: &'static str,
) -> Option<JsObject> {
    let value = object::get(global, heap, ctor_name)?;
    if let Some(ctor) = value.as_object() {
        object::get(ctor, heap, "prototype").and_then(|v| v.as_object())
    } else if let Some(native) = value.as_native_function() {
        native
            .own_property_descriptor(heap, "prototype")
            .ok()
            .flatten()
            .and_then(|d| match d.kind {
                crate::object::DescriptorKind::Data { value } => value.as_object(),
                _ => None,
            })
    } else {
        None
    }
}

/// Resolved well-known prototype handles for one realm.
#[derive(Debug, Default, Clone)]
pub(crate) struct RealmIntrinsics {
    /// `%Object.prototype%`.
    pub object_prototype: Option<JsObject>,
    /// `%Function.prototype%`.
    pub function_prototype: Option<JsObject>,
    /// `%Array.prototype%`.
    pub array_prototype: Option<JsObject>,
    /// `%Promise.prototype%`.
    pub promise_prototype: Option<JsObject>,
    /// `%RegExp.prototype%`. Needed so the flag accessors (§22.2.6.x
    /// step 3a) can return `undefined` instead of throwing when invoked
    /// with the prototype itself as the `this` value.
    pub regexp_prototype: Option<JsObject>,
    /// `%String.prototype%`. Lets a primitive-string method call resolve its
    /// builtin method through the shape-guarded own-data IC on this object
    /// instead of re-walking the constructor → prototype chain every call.
    pub string_prototype: Option<JsObject>,
    /// `%Number.prototype%`, for the same primitive-method IC on numbers.
    pub number_prototype: Option<JsObject>,
}

impl RealmIntrinsics {
    /// Populate every slot by walking `global_this`. Called once at the
    /// end of `build_global_this_impl` after every `BuiltinIntrinsic`
    /// has run.
    pub(crate) fn populate(&mut self, heap: &mut otter_gc::GcHeap, global: JsObject) {
        self.object_prototype = resolve_prototype(global, heap, "Object");
        self.function_prototype = resolve_prototype(global, heap, "Function");
        self.array_prototype = resolve_prototype(global, heap, "Array");
        self.promise_prototype = resolve_prototype(global, heap, "Promise");
        self.regexp_prototype = resolve_prototype(global, heap, "RegExp");
        self.string_prototype = resolve_prototype(global, heap, "String");
        self.number_prototype = resolve_prototype(global, heap, "Number");
    }

    /// Trace cached prototype handles as root slots.
    pub(crate) fn trace_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        for object in [
            &self.object_prototype,
            &self.function_prototype,
            &self.array_prototype,
            &self.promise_prototype,
            &self.regexp_prototype,
            &self.string_prototype,
            &self.number_prototype,
        ]
        .into_iter()
        .filter_map(Option::as_ref)
        {
            object.trace_gc_roots(visitor);
        }
    }

    /// All slots empty?
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.object_prototype.is_none()
            && self.function_prototype.is_none()
            && self.array_prototype.is_none()
            && self.promise_prototype.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Interpreter;
    use crate::runtime_state::RuntimeState;

    fn collect_minor_with_runtime_roots(interp: &mut Interpreter) {
        let mut roots = Vec::new();
        RuntimeState::new(interp).trace_roots(&mut |slot| roots.push(slot));
        interp
            .gc_heap_mut()
            .collect_minor_with_roots(&mut |visitor| {
                for &slot in &roots {
                    visitor(slot);
                }
            });
    }

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
        assert!(
            slots.promise_prototype.is_some(),
            "Promise.prototype cached"
        );
    }

    #[test]
    fn slot_matches_string_lookup_for_object_prototype() {
        let mut interp = Interpreter::new();
        let slot_proto = interp.realm_intrinsics().object_prototype.unwrap();
        let global = *interp.global_this();
        let walked = resolve_prototype(global, &mut interp.gc_heap, "Object").unwrap();
        assert_eq!(
            slot_proto, walked,
            "RealmIntrinsics slot must point at the same %Object.prototype% \
             that the global-walk resolves"
        );
    }

    #[test]
    fn slot_matches_string_lookup_for_function_prototype() {
        let mut interp = Interpreter::new();
        let slot_proto = interp.realm_intrinsics().function_prototype.unwrap();
        let global = *interp.global_this();
        let walked = resolve_prototype(global, &mut interp.gc_heap, "Function").unwrap();
        assert_eq!(
            slot_proto, walked,
            "RealmIntrinsics slot must point at the same %Function.prototype% \
             that the global-walk resolves"
        );
    }

    #[test]
    fn slots_are_forwarded_by_minor_gc() {
        let mut interp = Interpreter::new();
        let global = *interp.global_this();

        collect_minor_with_runtime_roots(&mut interp);

        let object_slot = interp.realm_intrinsics().object_prototype.unwrap();
        let function_slot = interp.realm_intrinsics().function_prototype.unwrap();
        let array_slot = interp.realm_intrinsics().array_prototype.unwrap();
        let promise_slot = interp.realm_intrinsics().promise_prototype.unwrap();
        assert_eq!(
            object_slot,
            resolve_prototype(global, &mut interp.gc_heap, "Object").unwrap(),
            "Object.prototype cache must be forwarded with globalThis"
        );
        assert_eq!(
            function_slot,
            resolve_prototype(global, &mut interp.gc_heap, "Function").unwrap(),
            "Function.prototype cache must be forwarded with globalThis"
        );
        assert_eq!(
            array_slot,
            resolve_prototype(global, &mut interp.gc_heap, "Array").unwrap(),
            "Array.prototype cache must be forwarded with globalThis"
        );
        assert_eq!(
            promise_slot,
            resolve_prototype(global, &mut interp.gc_heap, "Promise").unwrap(),
            "Promise.prototype cache must be forwarded with globalThis"
        );

        let obj = interp
            .alloc_host_object_with_roots(&[], &[])
            .expect("alloc object after scavenge");
        let cached_proto = interp.object_prototype_object_opt().unwrap();
        crate::object::set_prototype(obj, interp.gc_heap_mut(), Some(cached_proto));
        assert_eq!(
            crate::object::prototype(obj, interp.gc_heap()),
            Some(object_slot),
            "new objects must receive the forwarded cached Object.prototype"
        );
    }

    #[test]
    fn default_is_empty() {
        let slots = RealmIntrinsics::default();
        assert!(slots.is_empty(), "default RealmIntrinsics has no slots");
    }
}
