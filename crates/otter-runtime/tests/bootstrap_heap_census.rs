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
//! - a built runtime leaves the nursery empty, so the set a snapshot
//!   dumps is old space;
//! - only a handful of the interpreter's root sources are filled by a
//!   build, so only those need representing in a snapshot.

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
        natives.native_ref_count, natives.static_count,
        "every static body carries an external-reference index",
    );
    assert_eq!(
        natives.resolved_native_ref_count, natives.static_count,
        "every index must resolve back to the entry address its storage holds",
    );
    assert_eq!(
        natives.external_ref_table_len, natives.distinct_static_fns,
        "the table holds exactly the distinct static entries, interned once each",
    );
}

/// The external-reference index a body stores is a property of the install
/// sequence, not of where the process happened to load the binary. Two runtimes
/// built the same way in one process must agree on every index, or a snapshot
/// written by one could not be read by the other.
#[test]
fn external_ref_indices_are_stable_across_runtimes() {
    let first = full_surface_runtime().native_census();
    let second = full_surface_runtime().native_census();

    assert_eq!(first.external_ref_table_len, second.external_ref_table_len);
    assert_eq!(first.static_count, second.static_count);
    assert_eq!(first.distinct_static_fns, second.distinct_static_fns);
    assert_eq!(
        first.resolved_native_ref_count, second.resolved_native_ref_count,
        "both runtimes must resolve the same number of indices",
    );
    assert_eq!(
        first.resolved_native_ref_count, first.static_count,
        "and all of them",
    );
}

/// Where each API surface's objects land.
///
/// Everything a build allocates outlives the isolate, so the whole build
/// runs tenured — the interpreter's own builtin surface included, which
/// is why the nursery is empty on a freshly built runtime. This is the
/// property a snapshot writer rests on: the dump set is old space.
#[test]
fn a_built_runtime_leaves_nothing_in_the_nursery() {
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

    for (label, heap, natives) in &rows {
        assert_eq!(
            natives.outside_old_space, 0,
            "`{label}` left {} native callables outside old space",
            natives.outside_old_space,
        );
        assert_eq!(
            heap.young.object_count, 0,
            "`{label}` left {} objects ({} bytes) in the nursery; a page dump \
             of old space would miss them",
            heap.young.object_count, heap.young.live_bytes,
        );
        assert!(
            heap.old.object_count > 0,
            "`{label}` allocated nothing into old space",
        );
    }
    let full = &rows[4];
    assert!(
        full.2.in_old_space > rows[0].2.in_old_space,
        "the full surface must add natives on top of the base runtime",
    );
}

/// The root sources a finished build actually filled. Everything a
/// snapshot has to reproduce is in this list; everything absent from it
/// starts life after the build and needs no representation at all.
#[test]
fn build_fills_only_a_handful_of_root_sources() {
    let runtime = full_surface_runtime();
    let roots = runtime.root_census();
    println!("{}", roots.render_text());

    let occupied = roots.occupied();
    assert!(!occupied.is_empty(), "a build fills some root source");
    assert!(
        occupied.iter().any(|s| s.name == "global_this"),
        "globalThis is always a root",
    );
    assert!(
        occupied.len() < roots.sources.len(),
        "some root sources must still be empty after a build",
    );
}

/// Which bodies are not yet self-contained.
///
/// A page image carries page bytes and nothing else, so a body is
/// restorable only if it both keeps every GC reference inside the cage and
/// owns no storage the heap did not allocate. The second half is the one
/// a slot walk cannot see: a `Vec<String>` holds no GC handles, so no
/// tracer ever mentions it, and a restored copy is still a second owner of
/// the same buffer.
///
/// The allow-list below is the remaining work, and it only ever shrinks:
/// a type that appears here without being listed is a new way to make the
/// bootstrap graph unrestorable.
#[test]
fn only_the_known_body_types_are_not_yet_self_contained() {
    /// Body types that still own storage outside the heap. Delete an
    /// entry when its storage lands; never add one.
    const NOT_YET_SELF_CONTAINED: &[&str] = &[
        // The object sidecar itself: `Vec<String>` dictionary keys, an
        // `FxHashMap` index, `Vec<SlotMeta>`, and host payloads.
        "ExoticSlots",
        // The array sidecar: sparse/named/accessor tables and JSON
        // source bytes, in `HashMap`/`IndexMap`/`Vec`/`Arc` containers.
        "ArrayExoticSlots",
        // `SmallVec` captures, `Arc` closure payloads and trace hooks.
        "NativeFunctionBody",
        "JsRegExpBody",
    ];

    let mut runtime = full_surface_runtime();
    let audit = runtime.self_containment_audit();
    println!("{}", audit.render_text());

    assert!(
        audit.objects_audited > 1000,
        "the audit must have walked the built graph, saw {} objects",
        audit.objects_audited,
    );
    for row in &audit.rows {
        let leaf = row.type_name.rsplit("::").next().unwrap_or(row.type_name);
        assert!(
            NOT_YET_SELF_CONTAINED.contains(&leaf),
            "`{leaf}` is not self-contained ({} references outside the heap, \
             owns outside storage: {}); either move that storage into the heap \
             or the bootstrap image cannot carry it",
            row.escaping_slots,
            row.owns_outside_storage,
        );
    }
}
