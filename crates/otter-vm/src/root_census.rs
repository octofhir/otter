//! What a finished build leaves in the interpreter's root sources.
//!
//! [`crate::runtime_state::RuntimeState::trace_roots`] enumerates every
//! GC root the isolate holds, but it is a walker, not an inventory: it
//! tells the collector where the slots are and nothing about which
//! sources actually carry anything.
//!
//! A snapshot needs the inventory. Restoring a captured heap only
//! produces a working isolate if every root source that the build filled
//! is put back, and the way to know which those are is to count them on a
//! real build rather than reason about the code. Sources that come out
//! empty need no representation in a snapshot at all — which is most of
//! them, because module state, microtasks, timers, pending throws and JIT
//! bookkeeping all start life after the build ends.
//!
//! # Contents
//!
//! - [`RootSource`] — one named root source and how much it holds.
//! - [`RootCensus`] — every source, with the non-empty ones separated.
//! - [`Interpreter::root_census`] — take one.
//!
//! # Invariants
//!
//! - Every source [`crate::runtime_state`] walks appears here, so a new
//!   root source shows up in the inventory as soon as it is traced.
//! - Counts are slot counts, matching what the walker would visit; a
//!   source holding only null handles still counts them, because a
//!   snapshot has to reproduce the shape, not just the contents.
//!
//! # See also
//!
//! - [`otter_gc::census`] — the heap side of the same question.
//! - [`otter_gc::heap_image`] — what a captured old generation contains.

use crate::Interpreter;

/// One root source and the number of slots it currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootSource {
    /// Field or subsystem the slots belong to.
    pub name: &'static str,
    /// Slots the root walker would visit here.
    pub slots: u64,
}

/// Inventory of every root source after a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootCensus {
    /// Every source, in walk order.
    pub sources: Vec<RootSource>,
}

impl RootCensus {
    /// Sources holding at least one slot — the set a snapshot must
    /// reproduce.
    #[must_use]
    pub fn occupied(&self) -> Vec<RootSource> {
        self.sources
            .iter()
            .copied()
            .filter(|s| s.slots > 0)
            .collect()
    }

    /// Total slots across every source.
    #[must_use]
    pub fn total_slots(&self) -> u64 {
        self.sources.iter().map(|s| s.slots).sum()
    }

    /// Render as a deterministic text table: occupied sources first, then
    /// a single line naming the empty ones.
    #[must_use]
    pub fn render_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(1024);
        let occupied = self.occupied();
        let _ = writeln!(
            out,
            "; root sources — {} of {} occupied, {} slots total",
            occupied.len(),
            self.sources.len(),
            self.total_slots(),
        );
        for source in &occupied {
            let _ = writeln!(out, "  {:>8}  {}", source.slots, source.name);
        }
        let empty: Vec<&str> = self
            .sources
            .iter()
            .filter(|s| s.slots == 0)
            .map(|s| s.name)
            .collect();
        if !empty.is_empty() {
            let _ = writeln!(out, "  empty: {}", empty.join(", "));
        }
        out
    }
}

/// Count the slots behind one root-walking closure.
fn count(visit: impl FnOnce(&mut dyn FnMut(*mut otter_gc::raw::RawGc))) -> u64 {
    let mut slots = 0u64;
    let mut visitor = |_: *mut otter_gc::raw::RawGc| slots += 1;
    visit(&mut visitor);
    slots
}

impl Interpreter {
    /// Inventory every root source this isolate holds.
    ///
    /// Runs the same per-source walks the collector does, counting rather
    /// than tracing, so it observes exactly what a snapshot would have to
    /// reproduce.
    #[must_use]
    pub fn root_census(&self) -> RootCensus {
        use crate::gc_trace::GcTrace;

        let mut sources = Vec::new();
        let mut push = |name: &'static str, slots: u64| sources.push(RootSource { name, slots });

        push(
            "global_this",
            count(|v| self.global_this().trace_gc_roots(v)),
        );
        push(
            "realm_intrinsics",
            count(|v| self.realm_intrinsics().trace_roots(v)),
        );
        push(
            "extra_realms",
            count(|v| {
                for realm in self.extra_realms_for_trace() {
                    realm.trace_roots(v);
                }
            }),
        );
        push(
            "module_environments",
            count(|v| {
                for env in self.module_environments_for_trace() {
                    env.trace_gc_roots(v);
                }
            }),
        );
        push(
            "host_module_env_cache",
            count(|v| {
                for env in self.host_module_envs_for_trace() {
                    env.trace_gc_roots(v);
                }
            }),
        );
        push(
            "module_init_upvalues",
            count(|v| {
                for spine in self.module_init_upvalues_for_trace() {
                    for slot in spine.iter() {
                        v(slot as *const crate::UpvalueCell as *mut otter_gc::raw::RawGc);
                    }
                }
            }),
        );
        push(
            "template_objects",
            count(|v| {
                for value in self.template_objects_for_trace() {
                    value.trace_value_slots(v);
                }
            }),
        );
        push(
            "string_constant_cells",
            count(|v| {
                for value in self.string_constant_cells_for_trace() {
                    value.trace_value_slots(v);
                }
            }),
        );
        push(
            "small_int_string_cache",
            count(|v| {
                for value in self.small_int_strings_for_trace() {
                    value.trace_value_slots(v);
                }
            }),
        );
        push(
            "bigint_constant_cache",
            count(|v| {
                for value in self.bigint_constants_for_trace() {
                    value.trace_value_slots(v);
                }
            }),
        );
        push(
            "lean_callback_roots",
            count(|v| {
                for root in self.lean_callback_roots_for_trace() {
                    root.trace_slots(v);
                }
            }),
        );
        push("handle_arena", count(|v| self.handle_arena_trace(v)));
        push(
            "persistent_roots",
            count(|v| self.persistent_roots_for_trace().trace_gc_roots(v)),
        );
        push(
            "global_lexicals",
            count(|v| {
                for slot in self.global_lexicals_for_trace() {
                    v(slot as *const crate::UpvalueCell as *mut otter_gc::raw::RawGc);
                }
            }),
        );
        push(
            "global_lexical_load_ic",
            count(|v| {
                for slot in self.global_lexical_load_ic_for_trace() {
                    v(slot as *const crate::UpvalueCell as *mut otter_gc::raw::RawGc);
                }
            }),
        );
        push(
            "module_namespaces",
            count(|v| {
                for ns in self.module_namespaces_for_trace() {
                    ns.trace_gc_roots(v);
                }
            }),
        );
        push(
            "module_errors",
            count(|v| {
                for value in self.module_errors_for_trace() {
                    value.trace_value_slots(v);
                }
            }),
        );
        push(
            "module_async_init_promises",
            count(|v| {
                for promise in self.module_async_init_promises_for_trace() {
                    promise.trace_value_slots(v);
                }
            }),
        );
        push("microtasks", count(|v| self.microtasks().trace_gc_roots(v)));
        push(
            "timer_callbacks",
            count(|v| self.timer_callbacks().trace_gc_roots(v)),
        );
        push(
            "dynamic_import_registry",
            count(|v| self.dynamic_import_registry().trace_gc_roots(v)),
        );
        push(
            "symbol_registry",
            count(|v| self.symbol_registry_for_trace().trace_gc_roots(v)),
        );
        push(
            "well_known_symbols",
            count(|v| self.well_known_symbols_for_trace().trace_gc_roots(v)),
        );
        push(
            "error_classes",
            count(|v| self.error_classes_for_trace().trace_gc_roots(v)),
        );
        push(
            "function_user_props",
            count(|v| {
                for obj in self.function_user_props_for_trace() {
                    obj.trace_gc_roots(v);
                }
            }),
        );
        push(
            "function_prototype_overrides",
            count(|v| {
                for value in self.function_prototype_overrides_for_trace() {
                    value.trace_value_slots(v);
                }
            }),
        );
        push(
            "iterator_prototypes",
            count(|v| self.trace_iterator_prototypes(v)),
        );
        push(
            "function_kind_prototypes",
            count(|v| self.trace_function_kind_roots(v)),
        );
        push(
            "shape_runtime",
            count(|v| self.shape_runtime_for_trace().trace_roots(v)),
        );
        push(
            "simple_constructor_shapes",
            count(|v| {
                for shape in self.simple_constructor_shapes_for_trace() {
                    v(shape as *const crate::object::ShapeHandle as *mut otter_gc::raw::RawGc);
                }
            }),
        );
        push(
            "store_property_ics",
            count(|v| {
                for ic in self.store_property_ics_for_trace() {
                    ic.trace_roots(v);
                }
            }),
        );
        push(
            "pending_throws",
            count(|v| {
                if let Some(value) = self.pending_generator_throw_for_trace() {
                    value.trace_value_slots(v);
                }
                if let Some(value) = self.pending_uncaught_throw_for_trace() {
                    value.trace_value_slots(v);
                }
            }),
        );
        push(
            "iteration_anchors",
            count(|v| {
                for value in self.iteration_anchors_for_trace() {
                    value.trace_value_slots(v);
                }
            }),
        );
        push(
            "rejection_tracker",
            count(|v| self.rejection_tracker_for_trace().trace(v)),
        );
        push("register_stack", count(|v| self.trace_reg_stack(v)));
        push(
            "native_jit_activations",
            count(|v| self.trace_native_jit_activations(v)),
        );

        RootCensus { sources }
    }
}
