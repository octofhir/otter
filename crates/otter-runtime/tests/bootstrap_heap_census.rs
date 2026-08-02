//! What a full-surface bootstrap leaves on the heap.
//!
//! Prints the per-space type-tag census, the native-callable census,
//! and a per-API-surface breakdown of where the build's objects land.
//! Run with `--nocapture` to read the tables.
//!
//! The assertions pin the structural facts a bootstrap snapshot writer
//! depends on:
//!
//! - every native callable lands in exactly one dispatch-storage
//!   bucket, and static function pointers dominate;
//! - the closure-backed natives — the ones no dumped page can carry —
//!   are few enough to enumerate and re-install by name;
//! - **the bootstrap graph is not confined to old space.** The
//!   interpreter builds the whole ECMAScript builtin surface in
//!   `Interpreter::with_string_heap_cap` before the runtime turns on
//!   `set_tenure_all`, so that surface is allocated young. Only the
//!   extension, global-class, and JS-shim half of the build is
//!   tenured. A snapshot that dumps old-space pages alone would miss
//!   the builtins.

use otter_modules::OtterModulesBuilderExt;
use otter_node::NodeApiBuilderExt;
use otter_runtime::Runtime;
use otter_vm::native_census::{NativeCensus, NativeStorageKind};
use otter_web::WebApiBuilderExt;

fn base_runtime() -> Runtime {
    Runtime::builder().build().expect("base runtime")
}

fn full_surface_runtime() -> Runtime {
    Runtime::builder()
        .with_node_apis()
        .with_otter_modules()
        .with_web_apis()
        .build()
        .expect("full-surface runtime")
}

fn assert_buckets_are_consistent(natives: &NativeCensus) {
    assert_eq!(
        natives.static_count
            + natives.vm_intrinsic_count
            + natives.dynamic_count
            + natives.local_dynamic_count,
        natives.total,
        "every body lands in exactly one storage bucket",
    );
    assert_eq!(
        natives.in_old_space + natives.outside_old_space,
        natives.total,
        "every body lands in exactly one space bucket",
    );
    assert_eq!(
        natives.closures.iter().map(|r| r.count).sum::<u64>(),
        natives.dynamic_count + natives.local_dynamic_count,
        "the closure list must account for every closure-backed body",
    );
    assert!(
        natives
            .closures
            .iter()
            .all(|r| r.kind.needs_reinstall() && !r.name.is_empty()),
        "every closure row must be re-installable under a non-empty name",
    );
    assert!(
        natives
            .closures
            .iter()
            .all(|r| r.kind != NativeStorageKind::Static),
        "static bodies must never appear in the closure list",
    );
}

#[test]
fn full_surface_bootstrap_census() {
    let runtime = full_surface_runtime();
    let heap = runtime.heap_census();
    let natives = runtime.native_census();

    println!("{}", heap.render_text());
    println!("{}", natives.render_text());

    assert!(
        heap.old.object_count > 0,
        "the tenured half of the build allocated nothing into old space",
    );
    let row_bytes: u64 = heap.old.rows.iter().map(|r| r.bytes).sum();
    assert_eq!(
        row_bytes, heap.old.live_bytes,
        "old rows must sum to totals"
    );
    assert!(
        heap.old.rows.iter().all(|r| r.type_name != "?"),
        "every old-space tag must have a registered type name",
    );

    assert!(
        natives.total > 0,
        "a full surface installs native callables"
    );
    assert_buckets_are_consistent(&natives);
    assert!(
        natives.static_count > natives.dynamic_count + natives.local_dynamic_count,
        "static natives should dominate: static={} closures={}",
        natives.static_count,
        natives.dynamic_count + natives.local_dynamic_count,
    );
    assert_eq!(
        natives.jit_static_fn_count, natives.static_count,
        "every static body mirrors its entry address for JIT builtin guards",
    );
}

/// Where each API surface's objects land. The young population is
/// contributed entirely by the pre-tenure interpreter bootstrap: the
/// extension surfaces run inside the tenured window and add old-space
/// objects only.
#[test]
fn api_surfaces_add_only_tenured_objects() {
    let surfaces: [(&str, Runtime); 5] = [
        ("base", base_runtime()),
        (
            "+node",
            Runtime::builder()
                .with_node_apis()
                .build()
                .expect("runtime"),
        ),
        (
            "+otter_modules",
            Runtime::builder()
                .with_otter_modules()
                .build()
                .expect("runtime"),
        ),
        (
            "+web",
            Runtime::builder().with_web_apis().build().expect("runtime"),
        ),
        ("+all", full_surface_runtime()),
    ];

    println!(
        "{:<16} {:>8} {:>10} {:>8} {:>10} {:>8} {:>7} {:>7}",
        "surface", "old_obj", "old_bytes", "yng_obj", "yng_bytes", "natives", "old_nat", "yng_nat",
    );
    let mut rows = Vec::new();
    for (label, runtime) in &surfaces {
        let heap = runtime.heap_census();
        let natives = runtime.native_census();
        println!(
            "{label:<16} {:>8} {:>10} {:>8} {:>10} {:>8} {:>7} {:>7}",
            heap.old.object_count,
            heap.old.live_bytes,
            heap.young.object_count,
            heap.young.live_bytes,
            natives.total,
            natives.in_old_space,
            natives.outside_old_space,
        );
        assert_buckets_are_consistent(&natives);
        rows.push((*label, heap, natives));
    }

    let base_young_natives = rows[0].2.outside_old_space;
    assert!(
        base_young_natives > 0,
        "the interpreter builds its builtin surface before the tenured window",
    );
    for (label, _heap, natives) in &rows {
        assert_eq!(
            natives.outside_old_space, base_young_natives,
            "`{label}` changed the untenured native population; \
             API surfaces install inside the tenured window, so they must add \
             old-space natives only",
        );
    }
    let full = &rows[4];
    assert!(
        full.2.in_old_space > rows[0].2.in_old_space,
        "the full surface must add tenured natives on top of the base runtime",
    );
}
