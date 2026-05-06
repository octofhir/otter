//! Centralized bootstrap registry for global JavaScript surfaces.
//!
//! The registry owns the deterministic install order for globals,
//! constructors, namespaces, and host-provided surfaces. Entries
//! install through task-96 static specs and builders when available,
//! or through small placeholder installers for surfaces that have not
//! been migrated yet.
//!
//! # Contents
//! - [`BootstrapFeatures`] — install-time feature/capability gates.
//! - [`BootstrapEntry`] / [`BOOTSTRAP_ENTRIES`] — deterministic
//!   registry data.
//! - [`build_global_this`] — one-shot global object bootstrap.
//!
//! # Invariants
//! - The registry is a static ordered slice, not a hash map or
//!   closure registry in the JS hot path.
//! - Installers receive an explicit `&mut GcHeap` and global object.
//! - Duplicate global names are rejected by tests.
//! - Feature gates are checked at install time before allocation for
//!   an entry.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-global-object>
//! - [`docs/new-engine/tasks/96-production-js-surface-builders.md`](
//!     ../../../docs/new-engine/tasks/96-production-js-surface-builders.md
//!   )

use crate::js_surface::{Attr, JsSurfaceError, NamespaceBuilder};
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{Value, atomics, console, json, math};

/// Bootstrap feature/capability bitset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapFeatures(u32);

impl BootstrapFeatures {
    /// Core ECMAScript placeholder/builtin globals.
    pub const CORE: Self = Self(0b0001);
    /// Host console surface.
    pub const CONSOLE: Self = Self(0b0010);

    /// All default surfaces.
    #[must_use]
    pub const fn all() -> Self {
        Self(Self::CORE.0 | Self::CONSOLE.0)
    }

    /// Empty feature set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// `true` when all `required` bits are enabled.
    #[must_use]
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Return a copy without `feature`.
    #[must_use]
    pub const fn without(self, feature: Self) -> Self {
        Self(self.0 & !feature.0)
    }
}

/// One deterministic bootstrap entry.
pub struct BootstrapEntry {
    /// Global name installed by this entry.
    pub name: &'static str,
    /// Required feature/capability bits.
    pub feature: BootstrapFeatures,
    /// Installer function.
    pub install: BootstrapInstall,
}

/// Bootstrap installer function pointer.
pub type BootstrapInstall =
    fn(&BootstrapEntry, &mut otter_gc::GcHeap, JsObject) -> Result<(), JsSurfaceError>;

/// Deterministic global bootstrap registry.
pub static BOOTSTRAP_ENTRIES: &[BootstrapEntry] = &[
    placeholder("Array"),
    placeholder("Object"),
    BootstrapEntry {
        name: json::JSON_SPEC.name,
        feature: BootstrapFeatures::CORE,
        install: install_json,
    },
    placeholder("String"),
    placeholder("Number"),
    placeholder("Boolean"),
    placeholder("BigInt"),
    placeholder("Symbol"),
    BootstrapEntry {
        name: math::MATH_SPEC.name,
        feature: BootstrapFeatures::CORE,
        install: install_math,
    },
    placeholder("Date"),
    placeholder("RegExp"),
    placeholder("Map"),
    placeholder("Set"),
    placeholder("WeakMap"),
    placeholder("WeakSet"),
    placeholder("WeakRef"),
    placeholder("Promise"),
    placeholder("Proxy"),
    placeholder("Reflect"),
    placeholder("Function"),
    placeholder("ArrayBuffer"),
    placeholder("SharedArrayBuffer"),
    placeholder("DataView"),
    placeholder("Int8Array"),
    placeholder("Uint8Array"),
    placeholder("Uint8ClampedArray"),
    placeholder("Int16Array"),
    placeholder("Uint16Array"),
    placeholder("Int32Array"),
    placeholder("Uint32Array"),
    placeholder("Float32Array"),
    placeholder("Float64Array"),
    placeholder("BigInt64Array"),
    placeholder("BigUint64Array"),
    BootstrapEntry {
        name: atomics::ATOMICS_SPEC.name,
        feature: BootstrapFeatures::CORE,
        install: install_atomics,
    },
    placeholder("Intl"),
    placeholder("Temporal"),
    placeholder("AggregateError"),
    placeholder("FinalizationRegistry"),
    placeholder("Iterator"),
    BootstrapEntry {
        name: console::CONSOLE_SPEC.name,
        feature: BootstrapFeatures::CONSOLE,
        install: install_console,
    },
];

/// Build `globalThis` with all default features.
pub(crate) fn build_global_this(heap: &mut otter_gc::GcHeap) -> Result<JsObject, JsSurfaceError> {
    build_global_this_with_features(heap, BootstrapFeatures::all())
}

/// Build `globalThis` with explicit feature gates.
pub(crate) fn build_global_this_with_features(
    heap: &mut otter_gc::GcHeap,
    features: BootstrapFeatures,
) -> Result<JsObject, JsSurfaceError> {
    let global = object::alloc_object(heap)?;
    object::set(global, heap, "globalThis", Value::Object(global));
    for entry in BOOTSTRAP_ENTRIES {
        if features.contains(entry.feature) {
            (entry.install)(entry, heap, global)?;
        }
    }
    Ok(global)
}

const fn placeholder(name: &'static str) -> BootstrapEntry {
    BootstrapEntry {
        name,
        feature: BootstrapFeatures::CORE,
        install: install_placeholder,
    }
}

fn install_placeholder(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let placeholder = object::alloc_object(heap)?;
    let proto = object::alloc_object(heap)?;
    object::set(placeholder, heap, "prototype", Value::Object(proto));
    define_global(global, heap, entry.name, Value::Object(placeholder));
    Ok(())
}

fn install_math(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let namespace = NamespaceBuilder::from_spec(heap, &math::MATH_SPEC)
        .map_err(JsSurfaceError::from)?
        .build()?;
    define_global(global, heap, entry.name, Value::Object(namespace));
    Ok(())
}

fn install_json(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let namespace = NamespaceBuilder::from_spec(heap, &json::JSON_SPEC)?.build()?;
    define_global(global, heap, entry.name, Value::Object(namespace));
    Ok(())
}

fn install_atomics(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let namespace = NamespaceBuilder::from_spec(heap, &atomics::ATOMICS_SPEC)?.build()?;
    define_global(global, heap, entry.name, Value::Object(namespace));
    Ok(())
}

fn install_console(
    _entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    console::install(global, heap)
}

fn define_global(global: JsObject, heap: &mut otter_gc::GcHeap, name: &'static str, value: Value) {
    let descriptor = PropertyDescriptor::data(
        value,
        Attr::global_binding().writable,
        Attr::global_binding().enumerable,
        Attr::global_binding().configurable,
    );
    let _ = object::define_own_property(global, heap, name, descriptor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NativeFunction;

    #[test]
    fn registry_order_is_deterministic_and_unique() {
        let names: Vec<&str> = BOOTSTRAP_ENTRIES.iter().map(|entry| entry.name).collect();
        assert_eq!(names.first(), Some(&"Array"));
        assert_eq!(names.last(), Some(&"console"));

        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }

    #[test]
    fn feature_gates_skip_console() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let global = build_global_this_with_features(
            &mut heap,
            BootstrapFeatures::all().without(BootstrapFeatures::CONSOLE),
        )
        .expect("global");
        assert!(object::get(global, &heap, "Math").is_some());
        assert!(object::get(global, &heap, "console").is_none());
    }

    #[test]
    fn math_installs_with_static_native_methods_and_attrs() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let global = build_global_this(&mut heap).expect("global");
        let Value::Object(math) = object::get(global, &heap, "Math").expect("Math") else {
            panic!("Math should be an object")
        };

        let pi = object::get_own_descriptor(math, &heap, "PI").expect("PI");
        assert!(!pi.writable());
        assert!(!pi.enumerable());
        assert!(!pi.configurable());

        let Value::NativeFunction(abs) = object::get(math, &heap, "abs").expect("abs") else {
            panic!("Math.abs should be native")
        };
        assert!(NativeFunction::is_static_call(&abs, &heap));
        assert_eq!(abs.length(&heap), 1);
    }
}
