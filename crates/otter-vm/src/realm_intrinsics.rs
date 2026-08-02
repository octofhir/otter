//! Typed per-realm intrinsic slots.
//!
//! Boa-style typed registry: every well-known **prototype** that the
//! dispatch path looks up by name gets a dedicated slot. Bootstrap
//! runs once and caches the resolved handles; runtime lookups read
//! the slot directly instead of doing two `object::get()` calls
//! (global → ctor → prototype) on every call.
//!
//! # Contents
//! - [`Intrinsic`] — which prototype a slot holds.
//! - [`RealmIntrinsics`] — one nullable [`JsObject`] per [`Intrinsic`],
//!   plus a named accessor for each. Native-function-shaped constructors
//!   that are not on hot object-dispatch paths still resolve through
//!   `NativeFunction::own_property_descriptor`.
//!
//! # Invariants
//! - Slots are populated by reading the `globalThis` graph **after**
//!   `BOOTSTRAP_ENTRIES` finishes running. Each slot is the null handle
//!   until populate runs.
//! - Absence is the null handle, not an `Option` discriminant. That keeps
//!   a slot a plain 32-bit cage offset, so the same address is writable by
//!   the collector rewriting a moved handle and by a snapshot restore
//!   writing a relocated one. The accessors still hand callers an
//!   `Option`.
//! - The dispatch path treats a null slot as a cache miss and falls back
//!   to the original string-lookup helper.
//! - Slots are traced as runtime roots so moving GC rewrites the cached
//!   handles in place.

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

/// One cached realm prototype. The discriminant is the slot index, so the
/// slot order is the declaration order here and nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum Intrinsic {
    /// `%Object.prototype%`.
    ObjectPrototype,
    /// `%Function.prototype%`.
    FunctionPrototype,
    /// `%Array.prototype%`.
    ArrayPrototype,
    /// `%Promise.prototype%`.
    PromisePrototype,
    /// `%RegExp.prototype%`. Needed so the flag accessors (§22.2.6.x
    /// step 3a) can return `undefined` instead of throwing when invoked
    /// with the prototype itself as the `this` value.
    RegExpPrototype,
    /// `%Date.prototype%`. Internal clone/materialization paths use this
    /// canonical slot instead of consulting a mutable global constructor.
    DatePrototype,
    /// `%String.prototype%`. Lets a primitive-string method call resolve
    /// its builtin method through the shape-guarded own-data IC on this
    /// object instead of re-walking the constructor → prototype chain
    /// every call.
    StringPrototype,
    /// `%Number.prototype%`, for the same primitive-method IC on numbers.
    NumberPrototype,
    /// `%Map.prototype%`. Lets `map.get/set/has/delete` on an ordinary Map
    /// dispatch the builtin directly once the slot is confirmed pristine,
    /// skipping the per-call method-resolution walk and the native bridge.
    MapPrototype,
    /// `%Set.prototype%`, for the same direct dispatch of
    /// `set.add/has/delete`.
    SetPrototype,
}

impl Intrinsic {
    /// Every slot, in slot order.
    pub(crate) const ALL: [Self; Self::COUNT] = [
        Self::ObjectPrototype,
        Self::FunctionPrototype,
        Self::ArrayPrototype,
        Self::PromisePrototype,
        Self::RegExpPrototype,
        Self::DatePrototype,
        Self::StringPrototype,
        Self::NumberPrototype,
        Self::MapPrototype,
        Self::SetPrototype,
    ];

    /// Number of slots.
    pub(crate) const COUNT: usize = 10;

    /// Global constructor whose `.prototype` fills this slot.
    const fn constructor_name(self) -> &'static str {
        match self {
            Self::ObjectPrototype => "Object",
            Self::FunctionPrototype => "Function",
            Self::ArrayPrototype => "Array",
            Self::PromisePrototype => "Promise",
            Self::RegExpPrototype => "RegExp",
            Self::DatePrototype => "Date",
            Self::StringPrototype => "String",
            Self::NumberPrototype => "Number",
            Self::MapPrototype => "Map",
            Self::SetPrototype => "Set",
        }
    }
}

/// Resolved well-known prototype handles for one realm.
///
/// A slot is the null handle until bootstrap fills it. The accessors
/// below map that back to `None`.
#[derive(Debug, Clone)]
pub(crate) struct RealmIntrinsics {
    slots: [JsObject; Intrinsic::COUNT],
}

impl Default for RealmIntrinsics {
    fn default() -> Self {
        Self {
            slots: [JsObject::null(); Intrinsic::COUNT],
        }
    }
}

/// Emit one accessor per slot, so call sites keep reading a named
/// prototype rather than indexing an array.
macro_rules! intrinsic_accessors {
    ($($method:ident => $variant:ident,)*) => {
        impl RealmIntrinsics {
            $(
                #[doc = concat!("Cached `", stringify!($variant), "`, or `None` before bootstrap fills it.")]
                #[must_use]
                pub(crate) fn $method(&self) -> Option<JsObject> {
                    self.get(Intrinsic::$variant)
                }
            )*
        }
    };
}

intrinsic_accessors! {
    object_prototype => ObjectPrototype,
    function_prototype => FunctionPrototype,
    array_prototype => ArrayPrototype,
    promise_prototype => PromisePrototype,
    regexp_prototype => RegExpPrototype,
    date_prototype => DatePrototype,
    string_prototype => StringPrototype,
    number_prototype => NumberPrototype,
    map_prototype => MapPrototype,
    set_prototype => SetPrototype,
}

impl RealmIntrinsics {
    /// Read one slot, mapping the null handle to `None`.
    #[must_use]
    pub(crate) fn get(&self, slot: Intrinsic) -> Option<JsObject> {
        let handle = self.slots[slot as usize];
        (!handle.is_null()).then_some(handle)
    }

    /// Populate every slot by walking `global_this`. Called once at the
    /// end of `build_global_this_impl` after every `BuiltinIntrinsic`
    /// has run.
    pub(crate) fn populate(&mut self, heap: &mut otter_gc::GcHeap, global: JsObject) {
        for slot in Intrinsic::ALL {
            self.slots[slot as usize] = resolve_prototype(global, heap, slot.constructor_name())
                .unwrap_or_else(JsObject::null);
        }
    }

    /// Visit every slot address, filled or not.
    ///
    /// Unlike [`Self::trace_roots`] this does not skip empties: the walk
    /// has a fixed length and a fixed order, which is what lets a
    /// snapshot capture the slots into a list and write them back in the
    /// same order.
    pub(crate) fn visit_slots(&self, visitor: &mut GcRootVisitor<'_>) {
        for handle in &self.slots {
            let p = handle as *const JsObject as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
    }

    /// Trace cached prototype handles as root slots.
    pub(crate) fn trace_roots(&self, visitor: &mut GcRootVisitor<'_>) {
        for handle in &self.slots {
            if !handle.is_null() {
                handle.trace_gc_roots(visitor);
            }
        }
    }

    /// All slots empty?
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.iter().all(|handle| handle.is_null())
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
            })
            .expect("minor GC");
    }

    #[test]
    fn bootstrap_populates_well_known_slots() {
        let interp = Interpreter::new();
        let slots = &interp.realm_intrinsics();
        assert!(
            slots.object_prototype().is_some(),
            "Object.prototype cached"
        );
        assert!(
            slots.function_prototype().is_some(),
            "Function.prototype cached"
        );
        assert!(slots.array_prototype().is_some(), "Array.prototype cached");
        assert!(
            slots.promise_prototype().is_some(),
            "Promise.prototype cached"
        );
    }

    #[test]
    fn every_slot_is_visited_even_when_empty() {
        let empty = RealmIntrinsics::default();
        let mut visited = 0usize;
        empty.visit_slots(&mut |_| visited += 1);
        assert_eq!(
            visited,
            Intrinsic::COUNT,
            "a fixed-shape walk must not depend on which slots are filled"
        );
        let mut traced = 0usize;
        empty.trace_roots(&mut |_| traced += 1);
        assert_eq!(traced, 0, "the collector still skips empty slots");
    }

    #[test]
    fn slot_matches_string_lookup_for_object_prototype() {
        let mut interp = Interpreter::new();
        let slot_proto = interp.realm_intrinsics().object_prototype().unwrap();
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
        let slot_proto = interp.realm_intrinsics().function_prototype().unwrap();
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

        let object_slot = interp.realm_intrinsics().object_prototype().unwrap();
        let function_slot = interp.realm_intrinsics().function_prototype().unwrap();
        let array_slot = interp.realm_intrinsics().array_prototype().unwrap();
        let promise_slot = interp.realm_intrinsics().promise_prototype().unwrap();
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
