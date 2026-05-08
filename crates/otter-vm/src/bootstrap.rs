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

use crate::js_surface::{Attr, JsSurfaceError, NamespaceBuilder, NamespaceSpec, ObjectBuilder};
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{
    Value, array_prototype, array_statics, atomics, console, function_prototype, json, math,
    object_statics,
};

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
    BootstrapEntry {
        name: "Array",
        feature: BootstrapFeatures::CORE,
        install: install_array,
    },
    BootstrapEntry {
        name: object_statics::OBJECT_SPEC.name,
        feature: BootstrapFeatures::CORE,
        install: install_object,
    },
    BootstrapEntry {
        name: json::JSON_SPEC.name,
        feature: BootstrapFeatures::CORE,
        install: install_json,
    },
    placeholder("String"),
    BootstrapEntry {
        name: "Number",
        feature: BootstrapFeatures::CORE,
        install: install_number,
    },
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
    BootstrapEntry {
        name: "Function",
        feature: BootstrapFeatures::CORE,
        install: install_function,
    },
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

fn install_array(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let array = object::alloc_object(heap)?;
    let prototype = object::alloc_object(heap)?;
    object::set(array, heap, "prototype", Value::Object(prototype));
    {
        let mut builder = ObjectBuilder::from_object(heap, array);
        for method in array_statics::ARRAY_STATIC_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    {
        let mut builder = ObjectBuilder::from_object(heap, prototype);
        for method in array_prototype::ARRAY_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    define_global(global, heap, entry.name, Value::Object(array));
    Ok(())
}

fn install_number(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeFunction;
    use crate::{NativeCall, NativeCtx, NativeError};

    // Number.prototype with all the formatter methods + the
    // hidden `[[NumberData]]` slot (= +0 per §21.1.3) so
    // `Number.prototype.toString()` recovers the value.
    let prototype = object::alloc_object(heap)?;
    {
        let mut builder = ObjectBuilder::from_object(heap, prototype);
        for method in crate::number::prototype::NUMBER_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    object::set(
        prototype,
        heap,
        crate::number::prototype::NUMBER_DATA_SLOT_KEY,
        Value::Number(crate::number::NumberValue::from_i32(0)),
    );

    // §21.1.1 Number constructor. Both `Number(value)` (call) and
    // `new Number(value)` (construct) coerce `value` via §7.1.4
    // ToNumber. The construct form additionally wraps the result in
    // a `NumberObject` with `[[NumberData]] = ToNumber(value)`; the
    // pre-allocated receiver from `dispatch_construct` already has
    // `Number.prototype` linked as `[[Prototype]]`.
    fn number_ctor_call(
        ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let value = if args.is_empty() {
            crate::number::NumberValue::from_i32(0)
        } else {
            crate::number::NumberValue::from_f64(crate::number::parse::to_number_value(&args[0]))
        };
        if ctx.is_construct_call() {
            let this = ctx.this_value().clone();
            if let Value::Object(obj) = this {
                crate::object::set(
                    obj,
                    ctx.heap_mut(),
                    crate::number::prototype::NUMBER_DATA_SLOT_KEY,
                    Value::Number(value),
                );
                Ok(Value::Object(obj))
            } else {
                Err(NativeError::TypeError {
                    name: "Number",
                    reason: "expected object receiver in `new Number(...)`".to_string(),
                })
            }
        } else {
            Ok(Value::Number(value))
        }
    }

    let ctor_native = NativeFunction::new_static(heap, "Number", 1, number_ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    // The `Number` global itself is a GC-managed JsObject. Both the
    // constants/static methods and the `prototype` link sit on it
    // as ordinary properties; the callable+constructable surface is
    // wired through the dispatch path's hidden-slot lookup
    // (`__construct__` / `__call__` keys, see
    // `crate::object::CONSTRUCTOR_NATIVE_SLOT_KEY`).
    let statics = object::alloc_object(heap)?;
    // Chain `Number`'s statics to `Object.prototype` so the
    // prototype-resident methods (hasOwnProperty, toString,
    // isPrototypeOf, etc.) resolve through ordinary property
    // lookup. Object is installed earlier in BOOTSTRAP_ENTRIES, so
    // `Object.prototype` is already reachable.
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(statics, heap, Some(object_proto));
    }
    // Same chaining for `Number.prototype`, so
    // `Number.prototype.hasOwnProperty(...)` resolves.
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    // Wire the callable+constructable bridge: stash the native
    // ctor on the Number object under a reserved key the dispatch
    // path looks up before falling back to ordinary property load.
    object::set(
        statics,
        heap,
        crate::object::CONSTRUCTOR_NATIVE_SLOT_KEY,
        Value::NativeFunction(ctor_native),
    );
    // `Number.prototype` lives as an own property on the
    // constructor object (per §21.1.2.5). Spec posture is
    // `[[Writable]]: false, [[Enumerable]]: false,
    // [[Configurable]]: false`; ordinary `set` matches the first
    // and third but installs as enumerable — the descriptor surface
    // is tightened separately when the rest of the spec descriptors
    // get audited.
    object::set(statics, heap, "prototype", Value::Object(prototype));

    // §21.1.2 Number-namespace constants. Per spec, each is
    // `[[Writable]]: false, [[Enumerable]]: false,
    // [[Configurable]]: false` — install via `Attr::read_only()`
    // through the property builder so descriptor checks pass.
    let max_safe_int = ((1u64 << 53) - 1) as f64;
    let constants: &[(&'static str, f64)] = &[
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", 5e-324),
        ("EPSILON", f64::EPSILON),
        ("MAX_SAFE_INTEGER", max_safe_int),
        ("MIN_SAFE_INTEGER", -max_safe_int),
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
    ];
    {
        let mut builder = ObjectBuilder::from_object(heap, statics);
        for (name, value) in constants {
            builder.property(
                name,
                Value::Number(crate::number::NumberValue::from_f64(*value)),
                Attr::read_only(),
            )?;
        }
    }

    // Static predicates / parsers. Wired through dedicated native
    // callbacks that share the foundation `crate::number::parse`
    // implementation (the same helpers `Op::GlobalCall` reaches via
    // the compile-time alias for `Number.isNaN(x)` etc.).
    fn number_is_nan_native(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let result = matches!(args.first(), Some(Value::Number(n)) if n.as_f64().is_nan());
        Ok(Value::Boolean(result))
    }
    fn number_is_finite_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let result = matches!(args.first(), Some(Value::Number(n)) if n.as_f64().is_finite());
        Ok(Value::Boolean(result))
    }
    fn number_is_integer_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let v = args.first().cloned().unwrap_or(Value::Undefined);
        Ok(Value::Boolean(crate::number::parse::is_integer(&v)))
    }
    fn number_is_safe_integer_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let v = args.first().cloned().unwrap_or(Value::Undefined);
        Ok(Value::Boolean(crate::number::parse::is_safe_integer(&v)))
    }
    fn number_parse_int_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let s = match args.first() {
            Some(Value::String(s)) => s.to_lossy_string(),
            Some(other) => other.display_string(),
            None => return Ok(Value::Number(crate::number::NumberValue::from_f64(f64::NAN))),
        };
        let radix = match args.get(1) {
            Some(Value::Number(n)) => n.as_f64() as i32,
            _ => 0,
        };
        Ok(Value::Number(crate::number::parse::parse_int(&s, radix)))
    }
    fn number_parse_float_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let s = match args.first() {
            Some(Value::String(s)) => s.to_lossy_string(),
            Some(other) => other.display_string(),
            None => return Ok(Value::Number(crate::number::NumberValue::from_f64(f64::NAN))),
        };
        Ok(Value::Number(crate::number::parse::parse_float(&s)))
    }

    {
        let mut builder = ObjectBuilder::from_object(heap, statics);
        let methods: &[(&'static str, u8, crate::native_function::NativeFastFn)] = &[
            ("isNaN", 1, number_is_nan_native),
            ("isFinite", 1, number_is_finite_native),
            ("isInteger", 1, number_is_integer_native),
            ("isSafeInteger", 1, number_is_safe_integer_native),
            ("parseInt", 2, number_parse_int_native),
            ("parseFloat", 1, number_parse_float_native),
        ];
        for (name, length, call) in methods {
            builder.method(name, *length, NativeCall::Static(*call), Attr::builtin_function())?;
        }
    }

    let number_value = Value::Object(statics);
    // §21.1.3.1 `Number.prototype.constructor` points back at the
    // Number constructor.
    object::set(prototype, heap, "constructor", number_value.clone());
    define_global(global, heap, entry.name, number_value);
    Ok(())
}

fn install_function(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let function = object::alloc_object(heap)?;
    let prototype = object::alloc_object(heap)?;
    object::set(function, heap, "prototype", Value::Object(prototype));
    {
        let mut builder = ObjectBuilder::from_object(heap, prototype);
        for method in function_prototype::FUNCTION_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    function_prototype::install_restricted_accessors(heap, prototype)?;
    define_global(global, heap, entry.name, Value::Object(function));
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

fn install_object(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let object = object::alloc_object(heap)?;
    let prototype = object::alloc_object(heap)?;
    object::set(object, heap, "prototype", Value::Object(prototype));
    {
        let mut builder = ObjectBuilder::from_object(heap, object);
        for method in object_statics::OBJECT_SPEC.methods {
            builder.method_from_spec(method)?;
        }
    }
    {
        let mut builder = ObjectBuilder::from_object(heap, prototype);
        for method in object_statics::OBJECT_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    define_global(global, heap, entry.name, Value::Object(object));
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
        const MAX_DEFAULT_GC_ALLOCATIONS: u64 = 200;
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
        assert_eq!(telemetry.namespace_objects_installed(), 5);
        assert_eq!(telemetry.native_functions_installed(), 101);
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
