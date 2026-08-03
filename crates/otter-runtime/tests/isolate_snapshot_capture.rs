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

#[test]
fn restore_round_trips_in_process() {
    let source = full_surface_runtime();
    let snapshot = source.capture_isolate_snapshot().expect("capture");
    let mut restored = otter_vm::Interpreter::from_isolate_snapshot(&snapshot).expect("restore");

    // The restored global graph resolves the same intrinsics.
    let global = *restored.global_this();
    let object_ctor = otter_vm::object::get(global, restored.gc_heap(), "Object")
        .expect("restored global has Object");
    assert!(
        object_ctor.is_object_type(),
        "Object constructor survived the round trip"
    );
    let json = otter_vm::object::get(global, restored.gc_heap(), "JSON")
        .expect("restored global has JSON");
    assert!(json.is_object_type());

    // A full collection over the restored heap must find a consistent
    // graph: every reachable slot relocated, nothing double-owned.
    restored.force_gc().expect("full GC over restored heap");
    let after = otter_vm::object::get(global, restored.gc_heap(), "Object")
        .expect("Object survives a full GC");
    assert!(after.is_object_type());
}

#[test]
fn restored_runtime_evaluates_javascript() {
    let source = full_surface_runtime();
    let snapshot = source.capture_isolate_snapshot().expect("capture");
    let mut restored =
        otter_runtime::Runtime::from_isolate_snapshot(&snapshot).expect("runtime restore");

    let result = restored
        .eval(otter_runtime::SourceInput::from_javascript("1 + 1"))
        .expect("eval on restored runtime");
    assert_eq!(result.completion_string(), "2");

    let json = restored
        .eval(otter_runtime::SourceInput::from_javascript(
            "JSON.stringify({restored: [1, 2, 3], re: /a+/.test('caaat')})",
        ))
        .expect("JSON + RegExp on restored runtime");
    assert_eq!(
        json.completion_string(),
        r#"{"restored":[1,2,3],"re":true}"#
    );

    let steps: &[(&str, &str, &str)] = &[
        ("plain method", "({ tag() { return 'ok' } }).tag()", "ok"),
        ("map ctor", "new Map([[1,2]]).size.toString()", "1"),
        (
            "plain class",
            "class P { tag() { return 'ok' } } new P().tag()",
            "ok",
        ),
        (
            "extends proto wiring",
            "class E1 extends Map {}; (Object.getPrototypeOf(E1) === Map).toString()",
            "true",
        ),
        (
            "extends construct",
            "class E2 extends Map {}; (new E2() instanceof Map).toString()",
            "true",
        ),
        (
            "reflect construct",
            "Reflect.construct(Map, [], Map).size.toString()",
            "0",
        ),
        (
            "extends object with method",
            "class E3 extends Object { tag() { return 'ok' } } new E3().tag()",
            "ok",
        ),
        (
            "subclass instance canonical getter",
            "class Q8 extends Map {} new Q8().size.toString()",
            "0",
        ),
        ("eval hook", "eval('2 + 3').toString()", "5"),
        (
            "dynamic Function",
            "new Function('return \"fn-ok\"')()",
            "fn-ok",
        ),
        (
            "resizable ArrayBuffer",
            "const ab = new ArrayBuffer(8, {maxByteLength: 16}); ab.resize(16); ab.byteLength.toString()",
            "16",
        ),
        (
            "length-tracking TypedArray",
            "const ab2 = new ArrayBuffer(8, {maxByteLength: 16}); const ta = new Uint8Array(ab2); ab2.resize(12); ta.length.toString()",
            "12",
        ),
        (
            "subclass instance method (Map)",
            "class Q extends Map { tag() { return 'ok' } } new Q().tag()",
            "ok",
        ),
        (
            "subclass instance method (Set)",
            "class QS extends Set { tag() { return 'ok' } } new QS().tag()",
            "ok",
        ),
        (
            "subclass canonical through override chain",
            "class QM extends Map {} const m = new QM(); m.set(1, 2); m.get(1).toString()",
            "2",
        ),
    ];
    for (name, source, expected) in steps {
        let got = restored
            .eval(otter_runtime::SourceInput::from_javascript(*source))
            .unwrap_or_else(|err| panic!("{name} failed: {err}"));
        assert_eq!(got.completion_string(), *expected, "{name}");
    }
}

#[test]
fn snapshot_blob_round_trips_in_process() {
    let source = full_surface_runtime();
    let bytes = source.snapshot_blob().expect("blob capture");
    assert!(
        bytes.len() > 100_000,
        "the blob holds pages + bytecode, saw {} bytes",
        bytes.len()
    );

    // Same-process resolver: hand back the live isolate's closures by
    // their captured names.
    let dynamics = source.dynamic_natives_by_name();
    let mut restored = otter_runtime::Runtime::from_snapshot_blob_with(
        &bytes,
        otter_runtime::SnapshotRuntimeOptions::default(),
        &mut |name| {
            dynamics
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, payload)| payload.clone())
        },
    )
    .expect("blob restore");

    let result = restored
        .eval(otter_runtime::SourceInput::from_javascript(
            "JSON.stringify([1 + 1, /b+/.test('abbc'), new Map([[1, 2]]).get(1), eval('40 + 2')])",
        ))
        .expect("eval on blob-restored runtime");
    assert_eq!(result.completion_string(), "[2,true,2,42]");
}

#[test]
fn snapshot_cache_serves_the_second_build() {
    let cache_dir = tempfile::tempdir().expect("tempdir");

    let build = |dir: &std::path::Path| {
        otter_runtime::Runtime::builder()
            .with_node_apis()
            .with_otter_modules()
            .with_web_apis()
            .snapshot_cache_root(dir)
            .build()
            .expect("cached build")
    };

    let first = build(cache_dir.path());
    assert!(
        !first.restored_from_snapshot(),
        "an empty cache must bootstrap"
    );
    drop(first);

    let mut second = build(cache_dir.path());
    assert!(
        second.restored_from_snapshot(),
        "the second build must restore from the stored blob"
    );
    let probe = second
        .eval(otter_runtime::SourceInput::from_javascript(
            "JSON.stringify([1 + 1, typeof fetch, typeof Worker, typeof process.cwd(), \
             process.argv.length >= 0, eval('7 * 6')])",
        ))
        .expect("eval on cache-restored runtime");
    assert_eq!(
        probe.completion_string(),
        r#"[2,"function","function","string",true,42]"#
    );
}

#[test]
fn restore_is_cheaper_than_bootstrap() {
    use std::time::Instant;
    // Warm everything once, then capture the snapshot both paths share.
    let warm = full_surface_runtime();
    let snapshot = warm.capture_isolate_snapshot().expect("capture");

    const ROUNDS: u32 = 5;
    let build_started = Instant::now();
    for _ in 0..ROUNDS {
        let runtime = full_surface_runtime();
        std::hint::black_box(&runtime);
    }
    let build = build_started.elapsed() / ROUNDS;

    let restore_started = Instant::now();
    for _ in 0..ROUNDS {
        let runtime = otter_runtime::Runtime::from_isolate_snapshot(&snapshot).expect("restore");
        std::hint::black_box(&runtime);
    }
    let restore = restore_started.elapsed() / ROUNDS;

    eprintln!("SNAPSHOT-TIMING: build={build:?} restore={restore:?} per round over {ROUNDS}");
    assert!(
        restore < build,
        "restoring ({restore:?}) must beat rebuilding ({build:?})"
    );
}
