//! The snapshot capture side is deterministic: two identically-built
//! isolates produce the same atom table, the same fixed-root walk
//! length, and the same global-lexical key set. Offsets are cage
//! placement and may differ; identity of the *structure* must not.

use otter_modules::OtterModulesBuilderExt;
use otter_node::NodeApiBuilderExt;
use otter_web::WebApiBuilderExt;

fn full_surface_runtime() -> otter_runtime::Runtime {
    otter_runtime::Runtime::builder()
        .with_node_apis()
        .with_otter_modules()
        .with_web_apis()
        .build()
        .expect("full-surface runtime")
}

#[test]
fn capture_is_structurally_deterministic() {
    let a = full_surface_runtime();
    let b = full_surface_runtime();
    let snap_a = a.capture_isolate_snapshot().expect("capture a");
    let snap_b = b.capture_isolate_snapshot().expect("capture b");

    assert!(
        !snap_a.atom_names.is_empty(),
        "a built isolate interned property names"
    );
    assert_eq!(
        snap_a.atom_names, snap_b.atom_names,
        "atom table must be a function of the build, not of the run"
    );
    assert_eq!(
        snap_a.fixed_roots.len(),
        snap_b.fixed_roots.len(),
        "fixed root walk must have identical shape"
    );
    let keys_a: Vec<&str> = snap_a
        .global_lexicals
        .iter()
        .map(|(name, _, _)| name.as_ref())
        .collect();
    let keys_b: Vec<&str> = snap_b
        .global_lexicals
        .iter()
        .map(|(name, _, _)| name.as_ref())
        .collect();
    assert_eq!(keys_a, keys_b, "global lexical key set must match");
    assert!(
        snap_a.image.object_count() > 1000,
        "the image holds the bootstrap graph, saw {}",
        snap_a.image.object_count()
    );
}
