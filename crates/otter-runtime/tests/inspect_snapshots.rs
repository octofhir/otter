//! Phase 5.2 inspector surface coverage: IC dump, shape-transition
//! breakpoint observer, hidden-class snapshot, and frame / register
//! window snapshots driven through the step tracer.
//!
//! These tests build a [`Runtime`] directly (bypassing the worker
//! handle) so the inspector accessors can be called on the same
//! thread that mutates the VM.

use std::sync::{Arc, Mutex};

use otter_runtime::{
    Runtime, SourceInput,
    inspect::{
        FrameSnapshot, IcEntryVariant, IcSiteKind, IcSiteState, ShapeTransitionEvent,
        ShapeTransitionObserver, StepEvent, StepTracer,
    },
};

fn build_runtime() -> Runtime {
    Runtime::builder().build().expect("runtime build")
}

fn run(runtime: &mut Runtime, src: &str) {
    runtime
        .run_script(SourceInput::from_typescript(src), "<test>")
        .expect("script must succeed");
}

#[test]
fn ic_snapshot_reports_polymorphic_and_megamorphic_states() {
    let mut runtime = build_runtime();
    // Six receiver shapes all hit by the same property load site, so
    // the four-slot polymorphic cache overflows into megamorphic.
    let source = r#"
        function read(o) { return o.x; }
        const shapes = [
            { x: 1, a: 1 },
            { x: 2, b: 2 },
            { x: 3, c: 3 },
            { x: 4, d: 4 },
            { x: 5, e: 5 },
            { x: 6, f: 6 },
        ];
        for (let pass = 0; pass < 8; pass++) {
            for (const s of shapes) {
                read(s);
            }
        }
    "#;
    run(&mut runtime, source);

    let sites = runtime.ic_snapshot();
    let load_states: Vec<&IcSiteState> = sites
        .iter()
        .filter(|s| s.kind == IcSiteKind::Load)
        .map(|s| &s.state)
        .collect();
    assert!(
        !load_states.is_empty(),
        "expected at least one load IC site after script ran"
    );

    let mut saw_polymorphic = false;
    let mut saw_megamorphic = false;
    for state in load_states {
        match state {
            IcSiteState::Polymorphic { entries, .. } => {
                saw_polymorphic = true;
                assert!(
                    entries.len() <= 4,
                    "polymorphic cache must respect the four-entry cap"
                );
                assert!(
                    entries
                        .iter()
                        .all(|e| matches!(e.variant, IcEntryVariant::OwnData)),
                    "all load entries should be OwnData for this fixture"
                );
            }
            IcSiteState::Megamorphic => {
                saw_megamorphic = true;
            }
            IcSiteState::Empty => {}
        }
    }
    assert!(
        saw_polymorphic || saw_megamorphic,
        "expected at least one polymorphic or megamorphic state across load sites"
    );
    // Six distinct receiver shapes against a four-entry cache forces
    // the dominant load site to megamorphic. Tolerate the rare case
    // where a colder site stays polymorphic — the key invariant is
    // that megamorphic is reachable through the public API.
    assert!(
        saw_megamorphic,
        "load IC sites should reach megamorphic for six-shape mix"
    );
}

#[derive(Default)]
struct Recorder {
    inner: Mutex<Vec<ShapeTransitionEvent>>,
}

struct RecorderObserver(Arc<Recorder>);

impl ShapeTransitionObserver for RecorderObserver {
    fn on_transition(&mut self, event: &ShapeTransitionEvent) {
        self.0
            .inner
            .lock()
            .expect("recorder mutex")
            .push(event.clone());
    }
}

#[test]
fn shape_transition_observer_fires_on_property_adds() {
    let recorder = Arc::new(Recorder::default());
    let mut runtime = build_runtime();
    runtime.set_shape_transition_observer(Some(Box::new(RecorderObserver(recorder.clone()))));

    let source = r#"
        function build() {
            const o = {};
            o.alpha = 1;
            o.beta = 2;
            o.gamma = 3;
            return o;
        }
        build();
    "#;
    run(&mut runtime, source);

    let events = recorder.inner.lock().expect("recorder mutex").clone();
    assert!(
        events.iter().any(|e| e.key == "alpha"),
        "expected transition for `alpha`; got {events:?}"
    );
    assert!(
        events.iter().any(|e| e.key == "beta"),
        "expected transition for `beta`; got {events:?}"
    );
    assert!(
        events.iter().any(|e| e.key == "gamma"),
        "expected transition for `gamma`; got {events:?}"
    );

    let snapshot = runtime.shape_transition_snapshot();
    assert!(snapshot.nodes.len() >= 4, "root + 3 transitions minimum");
    let keys: Vec<String> = snapshot
        .nodes
        .iter()
        .filter_map(|n| n.transition_key.clone())
        .collect();
    assert!(keys.iter().any(|k| k == "alpha"));
    assert!(keys.iter().any(|k| k == "beta"));
    assert!(keys.iter().any(|k| k == "gamma"));
}

#[test]
fn shape_transition_observer_marks_cached_lookups_as_reused() {
    let recorder = Arc::new(Recorder::default());
    let mut runtime = build_runtime();
    runtime.set_shape_transition_observer(Some(Box::new(RecorderObserver(recorder.clone()))));

    // Two objects taking the same `{ x }` transition. First object
    // allocates the shape; second reuses the cached transition.
    let source = r#"
        const a = {};
        a.x = 1;
        const b = {};
        b.x = 2;
    "#;
    run(&mut runtime, source);

    let events = recorder.inner.lock().expect("recorder mutex").clone();
    let x_events: Vec<&ShapeTransitionEvent> = events.iter().filter(|e| e.key == "x").collect();
    assert!(
        x_events.iter().any(|e| !e.reused),
        "first `x` transition should report reused=false; got {x_events:?}"
    );
    assert!(
        x_events.iter().any(|e| e.reused),
        "second `x` transition should report reused=true; got {x_events:?}"
    );
}

struct CapturingTracer {
    events: Arc<Mutex<Vec<FrameSnapshot>>>,
}

impl StepTracer for CapturingTracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        // Capture only the synchronous main frame so the snapshot
        // remains deterministic; ignore frames pushed by built-in
        // method dispatch.
        if event.function_name == "<main>" {
            let snap = FrameSnapshot::from_step_event(event, false);
            self.events
                .lock()
                .expect("capture mutex")
                .push(snap);
        }
    }
}

#[test]
fn frame_snapshot_carries_register_window_for_main_frame() {
    let events: Arc<Mutex<Vec<FrameSnapshot>>> = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = build_runtime();
    runtime.set_tracer(Some(Box::new(CapturingTracer {
        events: events.clone(),
    })));

    run(&mut runtime, "const z = 41 + 1;\n");

    let snapshots = events.lock().expect("capture mutex").clone();
    assert!(!snapshots.is_empty(), "tracer should fire on main frame");
    assert!(
        snapshots.iter().any(|s| s.function_name == "<main>"),
        "expected at least one <main> frame snapshot"
    );
    // Late in the script the register window should hold the
    // computed `42`.
    let saw_42 = snapshots
        .iter()
        .any(|s| s.registers.iter().any(|r| r.debug == "number:42"));
    assert!(
        saw_42,
        "expected the computed `42` to appear in some captured register"
    );
}
