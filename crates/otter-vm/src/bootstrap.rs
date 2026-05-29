//! Centralized bootstrap registry for global JavaScript surfaces.
//!
//! The registry owns the deterministic install order for globals,
//! constructors, namespaces, and host-provided surfaces. Entries
//! install through static specs and builders when available,
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
//! - [JS surface builders](../../../docs/book/src/extensions/js-surface-builders.md)

use std::time::{Duration, Instant};

use crate::js_surface::{JsSurfaceError, NamespaceSpec};
use crate::object::{self, JsObject};
use crate::{
    Value, array_prototype, atomics, console, function_prototype, json, math, object_statics,
    reflect,
};

// Per-intrinsic installer helpers live in `crate::intrinsics::shared`;
// re-exported here so per-intrinsic modules + bootstrap call sites can
// import either path.
pub(crate) use crate::intrinsics::shared::{
    alloc_object_with_value_roots, install_placeholder, native_new_target_prototype,
};
pub use crate::intrinsics::shared::{
    alloc_object_with_value_roots_pub, define_global_value,
    native_constructor_static_with_value_roots, native_static_with_value_roots,
};

/// Bootstrap feature/capability bitset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapFeatures(u32);

impl BootstrapFeatures {
    /// Core ECMAScript placeholder/builtin globals.
    pub const CORE: Self = Self(0b0001);
    /// Host console surface.
    pub const CONSOLE: Self = Self(0b0010);
    /// Web Platform API globals (URL / Blob / Headers / Request /
    /// Response). Opt-in — runtime callers add it through
    /// `RuntimeBuilder::with_web_apis`.
    pub const WEB: Self = Self(0b0100);

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
///
/// Order is significant: every entry whose `install` callback links
/// its prototype to `Object.prototype` (via §19.1.2 / §23.1) must
/// come *after* `Object`. The current layout installs `Object` first
/// so subsequent entries can resolve `globalThis.Object.prototype`
/// without falling through to a null `[[Prototype]]`.
pub static BOOTSTRAP_ENTRIES: &[BootstrapEntry] = &[
    crate::bootstrap_entry!(crate::intrinsics::object::Intrinsic),
    crate::bootstrap_entry!(crate::intrinsics::array::Intrinsic),
    crate::bootstrap_entry!(crate::json::Intrinsic),
    crate::bootstrap_entry!(crate::string::intrinsic::Intrinsic),
    crate::bootstrap_entry!(crate::intrinsics::number::Intrinsic),
    crate::bootstrap_entry!(crate::boolean::intrinsic::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap_bigint::Intrinsic),
    crate::bootstrap_entry!(crate::intrinsics::symbol::Intrinsic),
    crate::bootstrap_entry!(crate::math::Intrinsic),
    crate::bootstrap_entry!(crate::intrinsics::date::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap_regexp::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap_collections::MapIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_collections::SetIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_collections::WeakMapIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_collections::WeakSetIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_weak_refs::WeakRefIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_promise::Intrinsic),
    crate::bootstrap_entry!(crate::intrinsics::proxy::Intrinsic),
    crate::bootstrap_entry!(crate::reflect::Intrinsic),
    crate::bootstrap_entry!(crate::intrinsics::function::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap_array_buffer::ArrayBufferIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_array_buffer::SharedArrayBufferIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_data_view::Intrinsic),
    // Abstract `%TypedArray%` must install before any per-kind ctor —
    // the per-kind couch! invocations resolve the abstract proto +
    // ctor via lookup, which would panic otherwise.
    crate::bootstrap_entry!(crate::bootstrap_typed_array::AbstractTypedArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Int8ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Uint8ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Uint8ClampedArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Int16ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Uint16ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Int32ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Uint32ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Float32ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Float64ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::BigInt64ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::BigUint64ArrayIntrinsic),
    crate::bootstrap_entry!(crate::atomics::Intrinsic),
    crate::bootstrap_entry!(crate::intrinsics::placeholders::IntlIntrinsic),
    crate::bootstrap_entry!(crate::intrinsics::placeholders::TemporalIntrinsic),
    crate::bootstrap_entry!(crate::intrinsics::placeholders::AggregateErrorIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_weak_refs::FinalizationRegistryIntrinsic),
    crate::bootstrap_entry!(crate::intrinsics::iterator::IteratorIntrinsic),
    crate::bootstrap_entry!(crate::console::Intrinsic),
    crate::bootstrap_entry!(crate::timers::Intrinsic),
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

    let global = alloc_object_with_value_roots(heap, &[])?;
    object::set(global, heap, "globalThis", Value::object(global));
    // §19.1 — `NaN`, `Infinity`, `undefined` are own properties of
    // the global object with writable / enumerable / configurable
    // all false. Reflective lookups (`Object.getOwnPropertyDescriptor(
    // globalThis, "NaN")`) observe the exact attributes.
    object::define_own_property(
        global,
        heap,
        "NaN",
        crate::object::PropertyDescriptor::data(Value::number_f64(f64::NAN), false, false, false),
    );
    object::define_own_property(
        global,
        heap,
        "Infinity",
        crate::object::PropertyDescriptor::data(
            Value::number_f64(f64::INFINITY),
            false,
            false,
            false,
        ),
    );
    object::define_own_property(
        global,
        heap,
        "undefined",
        crate::object::PropertyDescriptor::data(Value::undefined(), false, false, false),
    );
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
    if let Some(object_ctor) = object::get(global, heap, "Object").and_then(|v| v.as_object())
        && let Some(object_proto) =
            object::get(object_ctor, heap, "prototype").and_then(|v| v.as_object())
    {
        object::set_prototype(global, heap, Some(object_proto));
    }
    if let (Some(t), Some(before)) = (telemetry, before) {
        let after = allocation_snapshot(heap);
        t.finish_allocations(before, after);
    }
    Ok(global)
}

/// Per-kind iterator prototypes built off `%IteratorPrototype%`.
/// Returned from [`build_builtin_iterator_prototypes_post_bootstrap`]
/// so the caller can stash them on the [`crate::Interpreter`] cache
/// used by `intrinsic_prototype_object_for(Value::Iterator(_))`.
pub struct BuiltinIteratorPrototypes {
    /// `%ArrayIteratorPrototype%` — §23.1.5.2.
    pub array: JsObject,
    /// `%MapIteratorPrototype%` — §24.1.5.2.
    pub map: JsObject,
    /// `%SetIteratorPrototype%` — §24.2.5.2.
    pub set: JsObject,
    /// `%StringIteratorPrototype%` — §22.1.5.2.
    pub string: JsObject,
    /// `%RegExpStringIteratorPrototype%` — §22.2.7.2.
    pub regexp_string: JsObject,
}

/// §22.1.5.2 / §23.1.5.2 / §24.1.5.2 / §24.2.5.2 — materialise the
/// per-kind iterator prototypes. Each inherits from
/// `%IteratorPrototype%` and carries its own `@@toStringTag`. Caller
/// (`Interpreter::new`) caches the resulting `JsObject`s so
/// `[[GetPrototypeOf]]` on a `Value::Iterator` routes to the right
/// per-kind prototype.
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
        "Array" => {
            telemetry.entries_installed = telemetry.entries_installed.saturating_add(1);
            telemetry.objects_installed = telemetry.objects_installed.saturating_add(2);
            telemetry.prototype_objects_installed =
                telemetry.prototype_objects_installed.saturating_add(1);
            telemetry.native_functions_installed = telemetry
                .native_functions_installed
                .saturating_add(array_prototype::ARRAY_PROTOTYPE_METHODS.len());
        }
        "JSON" => telemetry.record_namespace(&json::JSON_SPEC),
        "Object" => telemetry.record_namespace(&object_statics::OBJECT_SPEC),
        "Math" => telemetry.record_namespace(&math::MATH_SPEC),
        "Atomics" => telemetry.record_namespace(&atomics::ATOMICS_SPEC),
        "Reflect" => telemetry.record_namespace(&reflect::REFLECT_SPEC),
        "Function" => {
            telemetry.entries_installed = telemetry.entries_installed.saturating_add(1);
            telemetry.objects_installed = telemetry.objects_installed.saturating_add(2);
            telemetry.prototype_objects_installed =
                telemetry.prototype_objects_installed.saturating_add(1);
            telemetry.native_functions_installed = telemetry
                .native_functions_installed
                .saturating_add(function_prototype::FUNCTION_PROTOTYPE_METHODS.len());
        }
        "Number" => {
            telemetry.entries_installed = telemetry.entries_installed.saturating_add(1);
            telemetry.objects_installed = telemetry.objects_installed.saturating_add(2);
            telemetry.prototype_objects_installed =
                telemetry.prototype_objects_installed.saturating_add(1);
            telemetry.native_functions_installed = telemetry
                .native_functions_installed
                .saturating_add(crate::number::prototype::NUMBER_PROTOTYPE_METHODS.len());
        }
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

// `install_iterator_well_knowns_post_bootstrap` is re-exported from
// `crate::intrinsics::iterator` for use by `lib.rs` / Symbol bootstrap.
pub(crate) use crate::intrinsics::iterator::install_iterator_well_knowns_post_bootstrap;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NativeFunction;

    #[test]
    fn registry_order_is_deterministic_and_unique() {
        let names: Vec<&str> = BOOTSTRAP_ENTRIES.iter().map(|entry| entry.name).collect();
        assert_eq!(names.first(), Some(&"Object"));
        assert_eq!(
            names.iter().position(|n| *n == "Array"),
            Some(1),
            "Array must install after Object so its [[Prototype]] can resolve"
        );
        assert_eq!(names.last(), Some(&"setTimeout"));
        assert!(
            names.contains(&"console"),
            "console entry must remain installed"
        );

        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }

    #[test]
    fn default_bootstrap_telemetry_matches_startup_ratchet() {
        // Bumped over time as the bootstrap installer family
        // grows: Map/Set/WeakMap/WeakSet (slice 9), Promise
        // (slice 10), RegExp (slice 11), WeakRef +
        // FinalizationRegistry (slice 12), `String.prototype`
        // method spec install pass (Iter 11),
        // `<Ctor>[Symbol.species]` accessors on Array / Map / Set /
        // RegExp / ArrayBuffer / SharedArrayBuffer plus
        // `Iterator.prototype.{next, return, throw}` installers.
        // Each ctor installs a `[[Construct]]` slot plus a prototype
        // with several native methods and (for some) accessors.
        // Bumped from 1100 → 1130 when the `Temporal` placeholder
        // was replaced with a real namespace carrying six sub-
        // namespaces (`Instant`, `Duration`, `PlainDate`, `PlainTime`,
        // `PlainDateTime`, `Now`) and their static methods.
        // Bumped from 1130 → 1280 when each `Temporal.<Class>` got a
        // populated `.prototype` carrying every per-class method as
        // an own data property (Tier 4 spec-conformance sweep) plus
        // the new `PlainYearMonth` class.
        // Bumped from 1280 → 1350 when `PlainMonthDay` + `ZonedDateTime`
        // landed, each shipping a constructor, statics, and a populated
        // prototype.
        // Bumped from 1350 → 1500 when every Temporal plain/zoned type
        // gained real `prototype` accessor properties (one native
        // getter closure + accessor descriptor per field) so branding
        // and prop-desc semantics match the spec.
        // Bumped from 1500 → 1550 for newly wired conversion methods
        // (toZonedDateTime, toPlainYearMonth/MonthDay, fromEpochNanoseconds,
        // toZonedDateTimeISO, Duration.with) each adding a prototype fn.
        const MAX_DEFAULT_GC_ALLOCATIONS: u64 = 1650;
        const MAX_DEFAULT_GC_ALLOCATED_BYTES: usize = 560 * 1024;

        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let mut telemetry = BootstrapTelemetry::default();
        let global =
            build_global_this_with_telemetry(&mut heap, BootstrapFeatures::all(), &mut telemetry)
                .expect("global");

        assert!(object::get(global, &heap, "Math").is_some());
        assert!(object::get(global, &heap, "Reflect").is_some());
        assert_eq!(telemetry.entries_considered(), BOOTSTRAP_ENTRIES.len());
        assert_eq!(telemetry.entries_installed(), BOOTSTRAP_ENTRIES.len());
        assert_eq!(telemetry.entries_skipped(), 0);
        assert_eq!(telemetry.duplicate_name_checks(), BOOTSTRAP_ENTRIES.len());
        assert_eq!(telemetry.duplicate_names_found(), 0);
        assert_eq!(telemetry.strings_interned(), 0);
        assert_eq!(telemetry.namespace_objects_installed(), 6);
        // 103 baseline + Object.is / Object.getPrototypeOf /
        // Object.setPrototypeOf (3) — all installed through the
        // `OBJECT_SPEC` namespace spec and therefore counted in
        // `native_functions_installed`. Date's static / prototype
        // methods are still installed via `ObjectBuilder::method_from_spec`
        // and don't bump the namespace counter. Array entry uses an
        // ad-hoc `record_installed_entry` branch that adds
        // `ARRAY_PROTOTYPE_METHODS.len()`, which grew by 3 with the
        // `copyWithin` / `toReversed` / `with` additions. Number
        // contributes `toLocaleString` as an own prototype builtin.
        assert_eq!(
            telemetry.native_functions_installed(),
            129 + reflect::REFLECT_SPEC.methods.len(),
        );
        assert!(
            telemetry.gc_allocations() <= MAX_DEFAULT_GC_ALLOCATIONS,
            "gc_allocations={} max={}",
            telemetry.gc_allocations(),
            MAX_DEFAULT_GC_ALLOCATIONS
        );
        assert!(
            telemetry.gc_allocated_bytes() <= MAX_DEFAULT_GC_ALLOCATED_BYTES,
            "gc_allocated_bytes={} max={}",
            telemetry.gc_allocated_bytes(),
            MAX_DEFAULT_GC_ALLOCATED_BYTES
        );
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
        let Some(math) = object::get(global, &heap, "Math")
            .expect("Math")
            .as_object()
        else {
            panic!("Math should be an object")
        };

        let pi = object::get_own_descriptor(math, &heap, "PI").expect("PI");
        assert!(!pi.writable());
        assert!(!pi.enumerable());
        assert!(!pi.configurable());

        let Some(abs) = object::get(math, &heap, "abs")
            .expect("abs")
            .as_native_function()
        else {
            panic!("Math.abs should be native")
        };
        assert!(NativeFunction::is_static_call(&abs, &heap));
        assert_eq!(abs.length(&heap), 1);
    }
}
