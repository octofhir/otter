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
    let diagnostics_a = snap_a.diagnostics();
    let diagnostics_b = snap_b.diagnostics();

    assert!(
        !diagnostics_a.atom_names().is_empty(),
        "a built isolate interned property names"
    );
    assert_eq!(
        diagnostics_a.atom_names(),
        diagnostics_b.atom_names(),
        "atom table must be a function of the build, not of the run"
    );
    assert_eq!(
        diagnostics_a.fixed_root_count(),
        diagnostics_b.fixed_root_count(),
        "fixed root walk must have identical shape"
    );
    assert_eq!(
        diagnostics_a.global_lexical_names(),
        diagnostics_b.global_lexical_names(),
        "global lexical key set must match"
    );
    assert!(
        diagnostics_a.object_count() > 1000,
        "the image holds the bootstrap graph, saw {}",
        diagnostics_a.object_count()
    );
}

#[test]
fn restore_round_trips_in_process() {
    let snapshot = {
        let source = full_surface_runtime();
        source.capture_isolate_snapshot().expect("capture")
    };
    let mut restored =
        otter_runtime::Runtime::from_isolate_snapshot(&snapshot).expect("runtime restore");

    let before = restored
        .eval(otter_runtime::SourceInput::from_javascript(
            "`${typeof Object},${typeof JSON},${typeof process.cwd},${typeof process.cwd()}`",
        ))
        .expect("restored globals and captured dynamic native");
    assert_eq!(
        before.completion_string(),
        "function,object,function,string"
    );

    // A full collection over the restored heap must find a consistent
    // graph: every reachable slot relocated, nothing double-owned.
    restored.force_gc().expect("full GC over restored heap");
    let after = restored
        .eval(otter_runtime::SourceInput::from_javascript(
            "`${typeof Object},${typeof JSON},${typeof process.cwd()}`",
        ))
        .expect("restored globals after full GC");
    assert_eq!(after.completion_string(), "function,object,string");
}

#[test]
fn restore_preserves_donor_authority_and_non_overridden_config() {
    let snapshot = {
        let donor = otter_runtime::Runtime::builder()
            .capabilities(otter_runtime::CapabilitySet::allow_all())
            .max_stack_depth(777)
            .process_global(false)
            .worker_global(false)
            .build()
            .expect("configured donor");
        donor.capture_isolate_snapshot().expect("capture")
    };

    let restored =
        otter_runtime::Runtime::from_isolate_snapshot(&snapshot).expect("runtime restore");
    assert!(restored.capabilities().is_allow_all());
    assert_eq!(restored.max_stack_depth(), 777);
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
        ("string match via @@match", "'abc'.match(/b/)[0]", "b"),
        (
            "regexp @@match presence",
            "(typeof RegExp.prototype[Symbol.match]).toString()",
            "function",
        ),
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
fn a_full_collection_on_a_restored_isolate_loses_nothing() {
    let mut source = full_surface_runtime();
    let snapshot = source.capture_isolate_snapshot().expect("capture");
    source.force_gc().expect("donor full GC");
    let donor = source.heap_census();
    let donor_rows: Vec<(u8, u64)> = donor
        .old
        .rows
        .iter()
        .map(|row| (row.type_tag, row.object_count))
        .collect();

    let mut restored =
        otter_runtime::Runtime::from_isolate_snapshot(&snapshot).expect("runtime restore");
    restored.force_gc().expect("restored full GC");
    let after = restored.heap_census();
    let after_by_tag: std::collections::HashMap<u8, u64> = after
        .old
        .rows
        .iter()
        .map(|row| (row.type_tag, row.object_count))
        .collect();
    // The restored isolate rebuilds its lookup caches empty, so bodies
    // the donor held ONLY through a cache legitimately die on the
    // first collection. What must never happen is a whole type
    // vanishing or a large bite out of one — that is the signature of
    // slots the relocation walk failed to rewrite (tagged compressed
    // words surfaced through stack temporaries, unvisited symbol
    // keys), which this test regresses.
    for (tag, donor_count) in &donor_rows {
        let after_count = after_by_tag.get(tag).copied().unwrap_or(0);
        assert!(
            after_count > 0,
            "type {tag:#x} vanished entirely after a restored-isolate GC ({donor_count} at donor)"
        );
        assert!(
            after_count * 2 >= *donor_count,
            "type {tag:#x} lost most of its bodies: donor {donor_count}, restored {after_count}"
        );
    }
}

#[test]
fn in_process_restored_isolate_survives_allocation_pressure() {
    let snapshot = {
        let source = full_surface_runtime();
        source.capture_isolate_snapshot().expect("capture")
    };
    let mut restored =
        otter_runtime::Runtime::from_isolate_snapshot(&snapshot).expect("runtime restore");
    assert!(restored.restored_from_snapshot());

    let probe = restored
        .eval(otter_runtime::SourceInput::from_javascript(
            "var junk; for (var i = 0; i < 400000; i++) { junk = {a: i, b: [i, i + 1]}; }             JSON.stringify(['abc'.match(/b/)[0], new Map([[1, 2]]).get(1),              typeof RegExp.prototype[Symbol.match], new Set([3]).has(3)])",
        ))
        .expect("pressure loop on restored runtime");
    assert_eq!(probe.completion_string(), r#"["b",2,"function",true]"#);

    restored.force_gc().expect("full GC");
    let again = restored
        .eval(otter_runtime::SourceInput::from_javascript(
            "JSON.stringify(['xyz'.match(/y/)[0], [1, 2, 3].map(function(v) { return v * 2; })])",
        ))
        .expect("post-GC eval");
    assert_eq!(again.completion_string(), r#"["y",[2,4,6]]"#);
}

#[test]
fn a_donor_finalization_registry_severs_on_restore() {
    // Seed the registry inside the bootstrap, the way an extension's
    // install script would: the donor captures under tenure-all, so
    // registry cells land in old space and ride the image.
    let snapshot = {
        let source = otter_runtime::Runtime::builder()
            .with_node_apis()
            .with_otter_modules()
            .with_web_apis()
            .extension_installer(otter_runtime::RuntimeExtensionInstaller::new(|ctx| {
                ctx.install_script(otter_runtime::SourceInput::from_javascript(
                    "globalThis.__fr = new FinalizationRegistry(function(){}); \
                     globalThis.__wr = new WeakRef(globalThis); \
                     for (var i = 0; i < 64; i++) { __fr.register({t: i}, i, {u: i}); }",
                ))
            }))
            .build()
            .expect("seeded runtime");
        source.capture_isolate_snapshot().expect("capture")
    };

    let mut restored =
        otter_runtime::Runtime::from_isolate_snapshot(&snapshot).expect("runtime restore");
    restored.force_gc().expect("full GC over severed registry");
    let probe = restored
        .eval(otter_runtime::SourceInput::from_javascript(
            "var junk; for (var i = 0; i < 200000; i++) { junk = {a: i}; }             JSON.stringify([typeof __fr, __wr.deref() === undefined || __wr.deref() === globalThis])",
        ))
        .expect("eval after severed-registry GC");
    assert_eq!(probe.completion_string(), r#"["object",true]"#);
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
