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
//! - [`BootstrapTelemetry`] — opt-in startup/bench counters.
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

use std::time::{Duration, Instant};

use crate::js_surface::{Attr, JsSurfaceError, NamespaceBuilder, NamespaceSpec};
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

/// One timed bootstrap phase captured by [`BootstrapTelemetry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapPhaseTelemetry {
    /// Phase label. Registry entry phases use the installed global name.
    pub name: &'static str,
    /// Wall-clock time spent in this phase.
    pub duration: Duration,
}

/// Default-off bootstrap counters for startup benchmarks and ratchets.
///
/// The normal runtime construction path does not allocate or update this
/// structure. Use [`build_global_this_with_telemetry`] from benches or focused
/// tests when a bootstrap change needs measured install counts.
#[derive(Debug, Clone, Default)]
pub struct BootstrapTelemetry {
    entries_considered: usize,
    entries_installed: usize,
    entries_skipped: usize,
    objects_installed: usize,
    prototype_objects_installed: usize,
    namespace_objects_installed: usize,
    native_functions_installed: usize,
    strings_interned: usize,
    gc_allocations: u64,
    gc_allocated_bytes: usize,
    duplicate_name_checks: usize,
    duplicate_names_found: usize,
    phases: Vec<BootstrapPhaseTelemetry>,
}

impl BootstrapTelemetry {
    /// Registry entries visited by the selected feature set.
    #[must_use]
    pub const fn entries_considered(&self) -> usize {
        self.entries_considered
    }

    /// Registry entries whose installer ran.
    #[must_use]
    pub const fn entries_installed(&self) -> usize {
        self.entries_installed
    }

    /// Registry entries skipped by feature gates.
    #[must_use]
    pub const fn entries_skipped(&self) -> usize {
        self.entries_skipped
    }

    /// Ordinary JS objects installed during bootstrap, including prototypes.
    #[must_use]
    pub const fn objects_installed(&self) -> usize {
        self.objects_installed
    }

    /// Prototype objects installed for placeholder constructor-shaped globals.
    #[must_use]
    pub const fn prototype_objects_installed(&self) -> usize {
        self.prototype_objects_installed
    }

    /// Namespace objects installed from static specs.
    #[must_use]
    pub const fn namespace_objects_installed(&self) -> usize {
        self.namespace_objects_installed
    }

    /// Native function objects installed from static specs.
    #[must_use]
    pub const fn native_functions_installed(&self) -> usize {
        self.native_functions_installed
    }

    /// Strings interned by the bootstrap path.
    #[must_use]
    pub const fn strings_interned(&self) -> usize {
        self.strings_interned
    }

    /// GC allocation count delta observed during bootstrap.
    #[must_use]
    pub const fn gc_allocations(&self) -> u64 {
        self.gc_allocations
    }

    /// GC live-byte delta observed during bootstrap.
    #[must_use]
    pub const fn gc_allocated_bytes(&self) -> usize {
        self.gc_allocated_bytes
    }

    /// Number of registry names checked for duplicates.
    #[must_use]
    pub const fn duplicate_name_checks(&self) -> usize {
        self.duplicate_name_checks
    }

    /// Duplicate registry names found during telemetry validation.
    #[must_use]
    pub const fn duplicate_names_found(&self) -> usize {
        self.duplicate_names_found
    }

    /// Timed bootstrap phases in execution order.
    #[must_use]
    pub fn phases(&self) -> &[BootstrapPhaseTelemetry] {
        &self.phases
    }

    fn reset(&mut self) {
        self.entries_considered = 0;
        self.entries_installed = 0;
        self.entries_skipped = 0;
        self.objects_installed = 0;
        self.prototype_objects_installed = 0;
        self.namespace_objects_installed = 0;
        self.native_functions_installed = 0;
        self.strings_interned = 0;
        self.gc_allocations = 0;
        self.gc_allocated_bytes = 0;
        self.duplicate_name_checks = 0;
        self.duplicate_names_found = 0;
        self.phases.clear();
    }

    fn push_phase(&mut self, name: &'static str, duration: Duration) {
        self.phases.push(BootstrapPhaseTelemetry { name, duration });
    }

    fn record_global_this(&mut self) {
        self.objects_installed = self.objects_installed.saturating_add(1);
    }

    fn record_placeholder(&mut self) {
        self.entries_installed = self.entries_installed.saturating_add(1);
        self.objects_installed = self.objects_installed.saturating_add(2);
        self.prototype_objects_installed = self.prototype_objects_installed.saturating_add(1);
    }

    fn record_namespace(&mut self, spec: &NamespaceSpec) {
        self.entries_installed = self.entries_installed.saturating_add(1);
        self.objects_installed = self.objects_installed.saturating_add(1);
        self.namespace_objects_installed = self.namespace_objects_installed.saturating_add(1);
        self.native_functions_installed = self
            .native_functions_installed
            .saturating_add(namespace_native_function_count(spec));
    }

    fn record_skipped_entry(&mut self) {
        self.entries_skipped = self.entries_skipped.saturating_add(1);
    }

    fn finish_allocations(&mut self, before: AllocationSnapshot, after: AllocationSnapshot) {
        self.gc_allocations = after.allocations.saturating_sub(before.allocations);
        self.gc_allocated_bytes = after.live_bytes.saturating_sub(before.live_bytes);
    }
}

#[derive(Debug, Clone, Copy)]
struct AllocationSnapshot {
    allocations: u64,
    live_bytes: usize,
}

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
///
/// This is primarily useful for startup benchmarks and feature-gate tests.
/// The default interpreter path calls [`build_global_this`].
pub fn build_global_this_with_features(
    heap: &mut otter_gc::GcHeap,
    features: BootstrapFeatures,
) -> Result<JsObject, JsSurfaceError> {
    build_global_this_impl(heap, features, None)
}

/// Build `globalThis` while collecting opt-in startup telemetry.
///
/// This entry point exists for Criterion benchmarks and task ratchets. The
/// production runtime path calls [`build_global_this`] and does not maintain
/// telemetry counters.
pub fn build_global_this_with_telemetry(
    heap: &mut otter_gc::GcHeap,
    features: BootstrapFeatures,
    telemetry: &mut BootstrapTelemetry,
) -> Result<JsObject, JsSurfaceError> {
    build_global_this_impl(heap, features, Some(telemetry))
}

fn build_global_this_impl(
    heap: &mut otter_gc::GcHeap,
    features: BootstrapFeatures,
    mut telemetry: Option<&mut BootstrapTelemetry>,
) -> Result<JsObject, JsSurfaceError> {
    let before = telemetry.as_deref_mut().map(|t| {
        t.reset();
        t.entries_considered = BOOTSTRAP_ENTRIES.len();
        let start = Instant::now();
        let duplicates = duplicate_name_count();
        t.duplicate_name_checks = BOOTSTRAP_ENTRIES.len();
        t.duplicate_names_found = duplicates;
        t.push_phase("duplicate-name-validation", start.elapsed());
        allocation_snapshot(heap)
    });

    let global = object::alloc_object(heap)?;
    object::set(global, heap, "globalThis", Value::Object(global));
    if let Some(t) = telemetry.as_deref_mut() {
        t.record_global_this();
    }
    for entry in BOOTSTRAP_ENTRIES {
        if features.contains(entry.feature) {
            let start = telemetry.as_ref().map(|_| Instant::now());
            (entry.install)(entry, heap, global)?;
            if let Some(t) = telemetry.as_deref_mut() {
                if let Some(start) = start {
                    t.push_phase(entry.name, start.elapsed());
                }
                record_installed_entry(t, entry);
            }
        } else if let Some(t) = telemetry.as_deref_mut() {
            t.record_skipped_entry();
        }
    }
    if let (Some(t), Some(before)) = (telemetry, before) {
        let after = allocation_snapshot(heap);
        t.finish_allocations(before, after);
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

fn namespace_native_function_count(spec: &NamespaceSpec) -> usize {
    spec.methods
        .len()
        .saturating_add(
            spec.accessors
                .iter()
                .filter(|accessor| accessor.get.is_some())
                .count(),
        )
        .saturating_add(
            spec.accessors
                .iter()
                .filter(|accessor| accessor.set.is_some())
                .count(),
        )
}

fn record_installed_entry(telemetry: &mut BootstrapTelemetry, entry: &BootstrapEntry) {
    match entry.name {
        "JSON" => telemetry.record_namespace(&json::JSON_SPEC),
        "Math" => telemetry.record_namespace(&math::MATH_SPEC),
        "Atomics" => telemetry.record_namespace(&atomics::ATOMICS_SPEC),
        "console" => telemetry.record_namespace(&console::CONSOLE_SPEC),
        _ => telemetry.record_placeholder(),
    }
}

fn duplicate_name_count() -> usize {
    let mut names: Vec<&str> = BOOTSTRAP_ENTRIES.iter().map(|entry| entry.name).collect();
    names.sort_unstable();
    names.windows(2).filter(|pair| pair[0] == pair[1]).count()
}

fn allocation_snapshot(heap: &mut otter_gc::GcHeap) -> AllocationSnapshot {
    let stats = heap.gc_stats();
    let allocations = stats.by_type.iter().map(|row| row.alloc_count_total).sum();
    AllocationSnapshot {
        allocations,
        live_bytes: stats.live_bytes,
    }
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
    fn default_bootstrap_telemetry_matches_startup_ratchet() {
        const MAX_DEFAULT_GC_ALLOCATIONS: u64 = 160;
        const MAX_DEFAULT_GC_ALLOCATED_BYTES: usize = 96 * 1024;

        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let mut telemetry = BootstrapTelemetry::default();
        let global =
            build_global_this_with_telemetry(&mut heap, BootstrapFeatures::all(), &mut telemetry)
                .expect("global");

        assert!(object::get(global, &heap, "Math").is_some());
        assert_eq!(telemetry.entries_considered(), BOOTSTRAP_ENTRIES.len());
        assert_eq!(telemetry.entries_installed(), BOOTSTRAP_ENTRIES.len());
        assert_eq!(telemetry.entries_skipped(), 0);
        assert_eq!(telemetry.duplicate_name_checks(), BOOTSTRAP_ENTRIES.len());
        assert_eq!(telemetry.duplicate_names_found(), 0);
        assert_eq!(telemetry.strings_interned(), 0);
        assert_eq!(telemetry.namespace_objects_installed(), 4);
        assert_eq!(telemetry.native_functions_installed(), 57);
        assert!(telemetry.gc_allocations() <= MAX_DEFAULT_GC_ALLOCATIONS);
        assert!(telemetry.gc_allocated_bytes() <= MAX_DEFAULT_GC_ALLOCATED_BYTES);
        assert_eq!(telemetry.phases().len(), BOOTSTRAP_ENTRIES.len() + 1);
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
