//! Phase 1 GC regression suite.
//!
//! This integration-test target collects the focused regressions
//! added while migrating `Value` bodies to `otter-gc` during tasks
//! 76–83. Keeping them under one test target gives closeout and
//! future GC refactors a compact command:
//! `cargo test -p otter-vm --test gc_phase1_regressions`.
//!
//! # Contents
//!
//! - Task 76: root enumeration and upvalue-cell reclamation.
//! - Task 77: object cycle reclamation.
//! - Task 78: array cycle reclamation and capacity cap accounting.
//! - Task 79: Map / Set cycle reclamation.
//! - Task 80: WeakMap / WeakSet ephemeron eviction.
//! - Task 81: WeakRef / FinalizationRegistry clearing and jobs.
//! - Task 82: Promise / iterator / generator graph tracing.
//! - Task 83: bound / native function and RegExp body tracing.
//!
//! # Invariants
//!
//! - Rooted values survive forced full GC.
//! - Unrooted cycles return their per-type live-byte counters to
//!   baseline.
//! - Heap-cap failures are reported as recoverable errors, not
//!   process aborts.
//!
// Origin task 76: root walker and first migrated GC body.
#[path = "gc_phase1_regressions/task76_roots.rs"]
mod task76_roots;

// Origin task 77: `JsObject` GC body and object-cycle reclamation.
#[path = "gc_phase1_regressions/task77_object_cycle.rs"]
mod task77_object_cycle;

// Origin task 78: `JsArray` GC body, array self-cycle, capacity cap.
#[path = "gc_phase1_regressions/task78_array_cycle.rs"]
mod task78_array_cycle;

// Origin task 79: `JsMap` / `JsSet` GC bodies and self-cycles.
#[path = "gc_phase1_regressions/task79_map_set_cycle.rs"]
mod task79_map_set_cycle;

// Origin task 80: `WeakMap` / `WeakSet` ephemeron fixpoint.
#[path = "gc_phase1_regressions/task80_weak_collections.rs"]
mod task80_weak_collections;

// Origin task 81: WeakRef clearing and FinalizationRegistry jobs.
#[path = "gc_phase1_regressions/task81_weakref_finalization.rs"]
mod task81_weakref_finalization;

// Origin task 82: Promise, iterator, generator, and parked-frame bodies.
#[path = "gc_phase1_regressions/task82_promise_iterator_generator.rs"]
mod task82_promise_iterator_generator;

// Origin task 83: bound/native function and RegExp bodies.
#[path = "gc_phase1_regressions/task83_bound_native_regexp.rs"]
mod task83_bound_native_regexp;
