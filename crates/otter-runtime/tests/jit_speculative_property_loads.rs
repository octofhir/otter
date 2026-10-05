//! Speculative monomorphic own-data loads in the optimizing tier.
//!
//! # Contents
//! - A warmed site completes in optimized code without exits or runtime
//!   property stubs, including when its loop is entered by OSR.
//! - A receiver of another shape exits a bounded number of times, and the
//!   function is compiled again by the optimizing tier and stops exiting.
//! - An accessor installed over the slot runs its getter exactly once per read.
//! - A loop that rewrites the receiver's shape keeps exact results.
//! - A callee spliced into its caller exits at its own site.
//! - Warm accessor and mapped-arguments sites without cache programs keep the
//!   optimized function running, with every getter and setter executed once.
//! - Deopt retrains an entire cold branch, independent recursive activations,
//!   and nested loops that must recover through OSR before returning.
//!
//! # Invariants
//! - Results match the interpreter oracle exactly.
//! - A failed speculation costs a bounded number of exits, never one per call.
//! - Native execution is proved by a source-function deopt or an isolated
//!   call whose main bytecodes are traced through completion, with no target
//!   bytecode or Template entry and the same current Graph generation.
//!   Graph call entries deliberately omit generated-entry accounting.
//! - Recovery inside a final long call requires a measured OSR entry after a
//!   separate warmup, where the probe has no other JavaScript loop.
//! - A script body compiled on a loop header leaves once at the first site
//!   after the loop that never ran before the compile; those exits belong to
//!   the script body, not to the measured site.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use otter_bytecode::Op;
use otter_runtime::{
    JitDebugCompileOutcome, JitDebugEvent, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    RuntimeExecutionStats, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::native_abi::ExitReason;

struct EntryProbe {
    name: &'static str,
    source: &'static str,
    expected: &'static str,
}

fn entry_probe(name: &'static str, source: &'static str, expected: &'static str) -> EntryProbe {
    EntryProbe {
        name,
        source,
        expected,
    }
}

/// Complete, unbounded trace of the small isolated probe. Only owned scalar
/// instruction identity is retained; no register values or VM handles escape.
struct ProbeTrace {
    events: Arc<Mutex<Vec<(u32, String, Op)>>>,
}

impl StepTracer for ProbeTrace {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        self.events.lock().unwrap().push((
            event.function_id,
            event.function_name.to_owned(),
            event.op,
        ));
    }
}

struct Run {
    completion: String,
    stats: RuntimeExecutionStats,
    events: Vec<JitDebugEvent>,
    /// Functions owning at least one optimizing-tier artifact bundle.
    optimized: Vec<String>,
    /// Exact installed generations entered by isolated probes, including
    /// copies of warmup-generation identities that may retire in the run.
    entry_probes: Vec<(String, u64)>,
    /// The probe of a split fixture contains only the measured callee's loops.
    osr_entries_after_setup: Option<u64>,
}

impl Run {
    fn function_names(&self) -> BTreeMap<u32, String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                JitDebugEvent::CompilePrepared {
                    function_id,
                    function_name,
                    ..
                } => Some((*function_id, function_name.clone())),
                _ => None,
            })
            .collect()
    }

    /// Event index and reason of every optimizing-tier exit taken by `name`.
    fn optimizing_exits(&self, name: &str) -> Vec<(usize, ExitReason)> {
        let names = self.function_names();
        self.events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| match event {
                JitDebugEvent::Bail {
                    function_name,
                    tier: JitDebugTier::Optimizing,
                    exit_reason,
                    ..
                } if function_name == name => Some((index, *exit_reason)),
                JitDebugEvent::EnteredGenerationDeopt {
                    callee_function_id,
                    callee_tier: JitDebugTier::Optimizing,
                    exit_reason,
                    ..
                } if names
                    .get(callee_function_id)
                    .is_some_and(|callee| callee == name) =>
                {
                    Some((index, *exit_reason))
                }
                _ => None,
            })
            .collect()
    }

    fn assert_optimized(&self, name: &str) {
        assert!(
            self.optimized.iter().any(|function| function == name),
            "{name} must own an optimizing artifact: {:?}",
            self.optimized
        );
        assert!(
            !self.optimizing_exits(name).is_empty()
                || self
                    .entry_probes
                    .iter()
                    .any(|(function, _)| function == name),
            "{name} needs an actual own native exit or an isolated entry probe"
        );
    }

    fn assert_spliced(&self, caller: &str, callee: &str, generation: Option<u64>) {
        let names = self.function_names();
        let owns = |fid: &u32| names.get(fid).is_some_and(|name| name == caller);
        assert!(
            self.events.iter().any(|event| matches!(event,
                JitDebugEvent::InlineLowered { function_id, code_object_id,
                    tier: JitDebugTier::Optimizing, parent_function_id, callee_function_id,
                    outcome: otter_vm::JitInlineLoweringOutcome::Inlined, .. }
                    if owns(function_id) && owns(parent_function_id)
                        && generation.is_none_or(|expected| expected == *code_object_id)
                        && names.get(callee_function_id).is_some_and(|name| name == callee)
            )),
            "{caller} generation {generation:?} must actually splice {callee}: {:?}",
            self.events
                .iter()
                .filter(|event| match event {
                    JitDebugEvent::InlineCandidate {
                        caller_function_id, ..
                    } => owns(caller_function_id),
                    JitDebugEvent::InlineLowered { function_id, .. }
                    | JitDebugEvent::CompilePrepared { function_id, .. }
                    | JitDebugEvent::CompileFinished { function_id, .. } => owns(function_id),
                    _ => false,
                })
                .take(32)
                .collect::<Vec<_>>()
        );
    }

    /// The script body leaves only at never-run sites, once per generation.
    fn assert_script_exits_only_for_unseen_sites(&self) {
        assert!(
            self.optimizing_exits("<main>")
                .iter()
                .all(|(_, reason)| *reason == ExitReason::InsufficientFeedback),
            "{:?}",
            self.optimizing_exits("<main>")
        );
    }

    /// After the last exit of `name`, the optimizing tier compiles it again
    /// and that generation completes the rest of the run without exiting.
    fn assert_recompiled_after_last_exit(&self, name: &str) {
        let exits = self.optimizing_exits(name);
        let Some(&(last_exit, _)) = exits.last() else {
            return;
        };
        let function_id = self
            .function_names()
            .into_iter()
            .find_map(|(id, function)| (function == name).then_some(id))
            .expect("compiled function");
        assert!(
            self.events[last_exit..].iter().any(|event| matches!(
                event,
                JitDebugEvent::CompileFinished {
                    function_id: compiled,
                    tier: JitDebugTier::Optimizing,
                    target,
                    outcome: JitDebugCompileOutcome::Compiled { code_object_id, .. },
                    ..
                } if *compiled == function_id && (
                    self.entry_probes.iter().any(|(function, generation)| function == name
                        && generation == code_object_id)
                    || matches!(target, otter_vm::jit_debug::JitDebugTarget::Osr { .. })
                        && self.osr_entries_after_setup.is_some_and(|entries| entries > 0)
                )
            )),
            "{name} must enter a replacement optimizing generation after {exits:?}"
        );
    }
}

fn run(source: &str, selection: JitSelection) -> Run {
    run_with_setup("", source, selection)
}

fn run_with_setup(setup: &str, source: &str, selection: JitSelection) -> Run {
    run_fixture(setup, source, selection, &[], &[])
}

fn run_fixture(
    setup: &str,
    source: &str,
    selection: JitSelection,
    setup_probes: &[EntryProbe],
    final_probes: &[EntryProbe],
) -> Run {
    let is_production = matches!(&selection, JitSelection::ProductionTiered);
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("speculative load runtime");
    let setup_result = (!setup.is_empty()).then(|| {
        runtime
            .run_script(
                SourceInput::from_javascript(setup),
                "speculative-load-setup.js",
            )
            .unwrap_or_else(|error| panic!("speculative load setup: {error:?}"))
    });
    let mut events: Vec<_> = setup_result
        .iter()
        .filter_map(|result| result.jit_debug_report())
        .flat_map(|report| report.events().iter().cloned())
        .collect();
    let mut entry_probes = Vec::new();
    run_entry_probes(
        &mut runtime,
        is_production,
        &events,
        setup_probes,
        &mut entry_probes,
    );
    let before_osr_entries = runtime.execution_stats().jit_optimized_osr_entries;
    let result = runtime
        .run_script(SourceInput::from_javascript(source), "speculative-load.js")
        .unwrap_or_else(|error| panic!("speculative load fixture: {error:?}"));
    events.extend(
        result
            .jit_debug_report()
            .into_iter()
            .flat_map(|report| report.events().iter().cloned()),
    );
    // Capture the actual run's counters before proof calls, which may execute
    // additional getters or alter feedback. A proof itself must not compile,
    // deopt or enter OSR, and cannot supply the run's semantic evidence.
    let stats = runtime.execution_stats();
    run_entry_probes(
        &mut runtime,
        is_production,
        &events,
        final_probes,
        &mut entry_probes,
    );
    let reports = setup_result.iter().chain(std::iter::once(&result));
    let optimized = reports
        .filter_map(|result| result.jit_artifacts())
        .flat_map(|artifacts| {
            artifacts
                .bundles()
                .iter()
                .filter(|bundle| bundle.manifest().tier() == JitDebugTier::Optimizing)
                .map(|bundle| bundle.manifest().function_name().to_owned())
        })
        .collect();
    Run {
        completion: result.completion_string().to_owned(),
        stats,
        events,
        optimized,
        entry_probes,
        osr_entries_after_setup: (!setup.is_empty()).then_some(
            stats
                .jit_optimized_osr_entries
                .saturating_sub(before_osr_entries),
        ),
    }
}

fn run_entry_probes(
    runtime: &mut Runtime,
    is_production: bool,
    events: &[JitDebugEvent],
    probes: &[EntryProbe],
    proven: &mut Vec<(String, u64)>,
) {
    let names: BTreeMap<_, _> = events
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::CompilePrepared {
                function_id,
                function_name,
                ..
            } => Some((*function_id, function_name.as_str())),
            _ => None,
        })
        .collect();
    for probe in probes {
        let generations = runtime.jit_code_generation_snapshot();
        let is_current_optimizer = |generation: &otter_vm::JitCodeGenerationSnapshot| {
            generation.tier == otter_vm::native_abi::NativeFrameKind::Optimizing
                && generation.lifecycle == otter_vm::native_abi::CodeLifetimeState::Installed
                && generation.linked
        };
        let target = generations.iter().find(|generation| {
            is_current_optimizer(generation)
                && names.get(&generation.function_id) == Some(&probe.name)
        });
        let current_template = |snapshots: &[otter_vm::JitCodeGenerationSnapshot], fid| {
            snapshots.iter().find_map(|generation| {
                (generation.function_id == fid
                    && generation.tier == otter_vm::native_abi::NativeFrameKind::Baseline
                    && generation.lifecycle == otter_vm::native_abi::CodeLifetimeState::Installed
                    && generation.linked)
                    .then_some((generation.code_object_id, generation.generated_entries))
            })
        };
        if is_production {
            assert!(target.is_some(), "{} needs a current optimizer", probe.name);
        }
        let before = runtime.execution_stats();
        let traced = Arc::new(Mutex::new(Vec::new()));
        runtime.set_tracer(Some(Box::new(ProbeTrace {
            events: traced.clone(),
        })));
        let result = runtime
            .run_script(
                SourceInput::from_javascript(probe.source),
                "speculative-entry-probe.js",
            )
            .unwrap_or_else(|error| panic!("{} entry probe: {error:?}", probe.name));
        runtime.set_tracer(None);
        let trace = traced.lock().unwrap();
        assert_eq!(result.completion_string(), probe.expected);
        let main_fid = trace
            .iter()
            .find_map(|(fid, name, _)| (name == "<main>").then_some(*fid))
            .expect("probe main must dispatch bytecode");
        assert!(
            trace
                .iter()
                .any(|(fid, _, op)| *fid == main_fid && matches!(op, Op::Call | Op::CallWithThis)),
            "main must dispatch its call: {trace:?}"
        );
        assert!(
            trace
                .iter()
                .any(|(fid, _, op)| *fid == main_fid
                    && matches!(op, Op::Return | Op::ReturnUndefined)),
            "main must dispatch completion: {trace:?}"
        );
        if !is_production {
            assert!(
                trace.iter().any(|(_, name, _)| name == probe.name),
                "the oracle must dispatch the measured callee: {trace:?}"
            );
            continue;
        }
        let after = runtime.execution_stats();
        let target = target.unwrap();
        assert!(
            !trace.iter().any(|(fid, _, _)| *fid == target.function_id),
            "the interpreter must not dispatch the measured callee: {trace:?}"
        );
        assert_eq!(
            after.jit_optimized_osr_entries,
            before.jit_optimized_osr_entries
        );
        assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
        assert_eq!(
            after.jit_generated_call_deopts,
            before.jit_generated_call_deopts
        );
        assert_eq!(after.jit_code_generations, before.jit_code_generations);
        let after_generations = runtime.jit_code_generation_snapshot();
        assert_eq!(
            current_template(&after_generations, target.function_id),
            current_template(&generations, target.function_id),
            "the same exact Template generation must remain present and unentered"
        );
        assert!(after_generations.iter().any(|generation| {
            generation.code_object_id == target.code_object_id && is_current_optimizer(generation)
        }));
        proven.push((probe.name.to_owned(), target.code_object_id));
    }
}

fn compare(source: &str, expected: &str) -> Run {
    let oracle = run(source, JitSelection::InterpreterOnly);
    assert_eq!(oracle.completion, expected);
    let compiled = run(source, JitSelection::ProductionTiered);
    assert_eq!(compiled.completion, oracle.completion);
    compiled
}

fn compare_with_setup(setup: &str, source: &str, expected: &str) -> Run {
    compare_with_entry_probes(setup, source, expected, &[], &[])
}

fn compare_with_entry_probes(
    setup: &str,
    source: &str,
    expected: &str,
    setup_probes: &[EntryProbe],
    final_probes: &[EntryProbe],
) -> Run {
    let oracle = run_fixture(
        setup,
        source,
        JitSelection::InterpreterOnly,
        setup_probes,
        final_probes,
    );
    assert_eq!(oracle.completion, expected);
    let compiled = run_fixture(
        setup,
        source,
        JitSelection::ProductionTiered,
        setup_probes,
        final_probes,
    );
    assert_eq!(compiled.completion, oracle.completion);
    compiled
}

#[test]
fn warmed_site_completes_without_exits_or_runtime_property_stubs() {
    const SOURCE: &str = r#"
const box = { value: 0.5, scale: 8 };
function scaled(count) {
    const local = box;
    let total = 0;
    for (let index = 0; index < count; index++) total += local.value * local.scale;
    return total;
}
let checksum = 0;
for (let call = 0; call < 300; call++) checksum += scaled(100);
checksum += scaled(200000);
String(checksum);
"#;
    let run = compare_with_entry_probes(
        "",
        SOURCE,
        "920000",
        &[],
        &[entry_probe("scaled", "String(scaled(100));", "400")],
    );
    run.assert_optimized("scaled");
    assert!(run.optimizing_exits("scaled").is_empty(), "{:?}", run.stats);
    run.assert_script_exits_only_for_unseen_sites();
    assert!(run.stats.jit_optimized_deopts <= 1, "{:?}", run.stats);
    assert_eq!(run.stats.jit_runtime_property_stubs, 0, "{:?}", run.stats);
}

#[test]
fn osr_entered_loop_reads_the_warmed_slot_without_exits() {
    const SOURCE: &str = r#"
const box = { value: 3, scale: 2 };
function once(count) {
    const local = box;
    let total = 0;
    for (let index = 0; index < count; index++) total += local.value * local.scale + (index & 1);
    return total;
}
String(once(400000));
"#;
    let run = compare(SOURCE, "2600000");
    assert!(run.stats.jit_optimized_osr_entries >= 1, "{:?}", run.stats);
    assert_eq!(run.stats.jit_optimized_deopts, 0, "{:?}", run.stats);
}

#[test]
fn another_shape_exits_a_bounded_number_of_times() {
    const SOURCE: &str = r#"
function read(record) {
    let total = 0;
    for (let index = 0; index < 64; index++) total += record.value;
    return total;
}
const first = { value: 1 };
for (let warm = 0; warm < 400; warm++) read(first);
const second = { padding: 0, value: 2 };
let checksum = 0;
for (let call = 0; call < 400; call++) checksum += read(second) + read(first);
String(checksum);
"#;
    let run = compare_with_entry_probes(
        "",
        SOURCE,
        "76800",
        &[],
        &[entry_probe("read", "String(read(second));", "128")],
    );
    run.assert_optimized("read");
    let exits = run.optimizing_exits("read");
    assert!(
        exits.len() <= 2,
        "a failed shape speculation must not exit per call: {exits:?}"
    );
    run.assert_recompiled_after_last_exit("read");
    run.assert_script_exits_only_for_unseen_sites();
}

#[test]
fn accessor_over_the_slot_runs_its_getter_once_per_read() {
    const SOURCE: &str = r#"
function read(record) { return record.value; }
const record = { value: 1 };
let warmed = 0;
for (let warm = 0; warm < 2000; warm++) warmed += read(record);
let calls = 0;
Object.defineProperty(record, "value", { get() { calls++; return 5; } });
let total = 0;
for (let call = 0; call < 500; call++) total += read(record);
JSON.stringify([warmed, total, calls]);
"#;
    let run = compare(SOURCE, "[2000,2500,500]");
    assert!(run.stats.jit_optimized_deopts <= 2, "{:?}", run.stats);
}

#[test]
fn loop_rewriting_the_receiver_shape_keeps_exact_results() {
    const SOURCE: &str = r#"
function churn(record, count) {
    let total = 0;
    for (let index = 0; index < count; index++) {
        total += record.value;
        if (index === count - 3) {
            delete record.value;
            record.extra = 1;
            record.value = 100;
        }
    }
    return total;
}
let checksum = 0;
for (let call = 0; call < 300; call++) checksum += churn({ value: 1 }, 64);
String(checksum);
"#;
    compare(SOURCE, "78600");
}

#[test]
fn inlined_callee_exits_at_its_own_site() {
    const SOURCE: &str = r#"
const receiver = {
    bias: 4,
    apply(value) { return value + this.bias; },
};
function drive(target, count) {
    let total = 0;
    for (let index = 0; index < count; index++) total += target.apply(index);
    return total;
}
let checksum = 0;
for (let call = 0; call < 300; call++) checksum += drive(receiver, 64);
const moved = { extra: 1, bias: 7, apply: receiver.apply };
for (let call = 0; call < 300; call++) checksum += drive(moved, 64) + drive(receiver, 64);
String(checksum);
"#;
    let run = compare_with_entry_probes(
        "",
        SOURCE,
        "2102400",
        &[],
        &[entry_probe("drive", "String(drive(moved, 0));", "0")],
    );
    run.assert_optimized("drive");
    let exits = run.optimizing_exits("drive");
    assert!(exits.len() <= 2, "{exits:?}");
    run.assert_recompiled_after_last_exit("drive");
    assert!(run.stats.jit_optimized_deopts <= 4, "{:?}", run.stats);
}

#[test]
fn warmed_uncacheable_accessor_sites_do_not_cascade_feedback_exits() {
    const SOURCE: &str = r#"
let writes = 0;
let reads = 0;
const proto = {
    set a(value) { writes += value; }, get a() { reads++; return 3; },
    set b(value) { writes += value; }, get b() { reads++; return 3; },
    set c(value) { writes += value; }, get c() { reads++; return 3; },
    set d(value) { writes += value; }, get d() { reads++; return 3; },
    set e(value) { writes += value; }, get e() { reads++; return 3; },
    set f(value) { writes += value; }, get f() { reads++; return 3; },
    set g(value) { writes += value; }, get g() { reads++; return 3; },
    set h(value) { writes += value; }, get h() { reads++; return 3; },
};
const record = Object.create(proto);
function fill(target, value) {
    target.a = value; target.b = value; target.c = value; target.d = value;
    target.e = value; target.f = value; target.g = value; target.h = value;
    return target.a + target.b + target.c + target.d + target.e + target.f + target.g + target.h;
}
let total = 0;
for (let call = 0; call < 2000; call++) total += fill(record, 1);
JSON.stringify([total, writes, reads]);
"#;
    let run = compare_with_entry_probes(
        "",
        SOURCE,
        "[48000,16000,16000]",
        &[],
        &[entry_probe("fill", "String(fill(record, 1));", "24")],
    );
    run.assert_optimized("fill");
    assert!(
        run.optimizing_exits("fill").is_empty(),
        "executed accessor sites must not be mistaken for cold branches: {:?}",
        run.optimizing_exits("fill")
    );
}

#[test]
fn warmed_mapped_arguments_property_sites_do_not_exit_for_empty_caches() {
    const SOURCE: &str = r#"
function mapped(value) {
    arguments.marker = value;
    return arguments.marker;
}
let total = 0;
for (let call = 0; call < 3000; call++) total += mapped(call & 7);
String(total);
"#;
    let run = compare_with_entry_probes(
        "",
        SOURCE,
        "10500",
        &[],
        &[entry_probe("mapped", "String(mapped(3));", "3")],
    );
    run.assert_optimized("mapped");
    assert!(
        run.optimizing_exits("mapped").is_empty(),
        "mapped arguments executed the source site: {:?}",
        run.optimizing_exits("mapped")
    );
}

#[test]
fn deopt_retrains_all_sites_in_a_new_branch_before_recompiling() {
    const SOURCE: &str = r#"
function branch(record, cold) {
    if (!cold) return record.a + arguments.length;
    return record.b + record.c + record.d + record.e + record.f + arguments.length;
}
const record = { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6 };
let warmed = 0;
for (let call = 0; call < 2000; call++) warmed += branch(record, false);
const first = branch(record, true);
let total = 0;
for (let call = 0; call < 2000; call++) total += branch(record, true);
JSON.stringify([warmed, first, total]);
"#;
    let run = compare_with_entry_probes(
        "",
        SOURCE,
        "[6000,22,44000]",
        &[],
        &[entry_probe("branch", "String(branch(record, true));", "22")],
    );
    run.assert_optimized("branch");
    let exits = run.optimizing_exits("branch");
    assert_eq!(
        exits.len(),
        1,
        "one cold branch must not cascade at its later sites: {exits:?}"
    );
    assert_eq!(exits[0].1, ExitReason::InsufficientFeedback);
    run.assert_recompiled_after_last_exit("branch");
}

#[test]
fn deopt_retraining_keeps_same_function_recursive_frames_independent() {
    const SOURCE: &str = r#"
function walk(depth, record) {
    if (depth === 0) return record.value;
    return walk(depth - 1, record) + 1;
}
const first = { value: 1 };
let warmed = 0;
for (let call = 0; call < 2000; call++) warmed += walk(3, first);
const changed = { extra: 0, value: 5 };
let total = 0;
for (let call = 0; call < 2000; call++) total += walk(3, changed);
JSON.stringify([warmed, total]);
"#;
    let run = compare_with_entry_probes(
        "",
        SOURCE,
        "[8000,16000]",
        &[],
        &[entry_probe("walk", "String(walk(0, changed));", "5")],
    );
    run.assert_optimized("walk");
    // The new shape may first reach an optimized caller that inlined `walk`
    // and retrains it there, so `walk`'s own body exits at most twice.
    let exits = run.optimizing_exits("walk");
    assert!(
        exits.len() <= 2,
        "recursive deopt must remain bounded: {exits:?}"
    );
    assert!(
        exits
            .iter()
            .all(|(_, reason)| *reason == ExitReason::ShapeGuard)
    );
    if !exits.is_empty() {
        run.assert_recompiled_after_last_exit("walk");
    }
}

#[test]
fn deopt_retraining_in_alternating_nested_loops_recovers_before_return() {
    const SETUP: &str = r#"
function nested(record, count, change) {
    let total = 0;
    for (let outer = 0; outer < count; outer++) {
        for (let inner = 0; inner < 1; inner++) total += record.value;
        if (change && outer === 64) {
            record.extra = 0;
            record.value = 3;
        }
    }
    return total;
}
let warmed = 0;
for (let call = 0; call < 2000; call++) warmed += nested({ value: 1 }, 2, false);
"#;
    const SOURCE: &str = r#"
const total = nested({ value: 1 }, 20000, true);
JSON.stringify([warmed, total]);
"#;
    let run = compare_with_setup(SETUP, SOURCE, "[4000,59870]");
    run.assert_optimized("nested");
    let exits = run.optimizing_exits("nested");
    assert!(
        !exits.is_empty() && exits.len() <= 2,
        "nested loop deopt must remain bounded: {exits:?}"
    );
    let function_id = run
        .function_names()
        .into_iter()
        .find_map(|(id, name)| (name == "nested").then_some(id))
        .unwrap();
    let last_exit = exits.last().unwrap().0;
    assert!(
        run.events[last_exit..].iter().any(|event| matches!(
                event,
                JitDebugEvent::CompileFinished {
                    function_id: compiled,
                    tier: JitDebugTier::Optimizing,
                    target: otter_vm::jit_debug::JitDebugTarget::Osr { .. },
                    outcome: JitDebugCompileOutcome::Compiled { .. },
                    ..
                } if *compiled == function_id
        )),
        "the final long invocation must recover through optimizing OSR: {exits:?}"
    );
    assert!(
        run.osr_entries_after_setup
            .is_some_and(|entries| entries > 0),
        "the final call must enter optimizing OSR, not just compile it: {:?}",
        run.stats
    );
}

#[test]
fn retraining_callee_invalidates_both_spliced_callers_without_stale_inline_entry() {
    const SETUP: &str = r#"
function leaf(record) { return record.value + 1; }
function left(record) { return leaf(record) + arguments.length; }
function right(record) { return leaf(record) + arguments.length + 1; }
const original = { value: 1 };
let direct = 0;
for (let call = 0; call < 2000; call++) direct += leaf(original);
let warmed = 0;
for (let call = 0; call < 2000; call++) warmed += left(original) + right(original);
"#;
    const SOURCE: &str = r#"
const changed = { extra: 0, value: 5 };
const first = leaf(changed);
let total = 0;
let retrained = 0;
for (let call = 0; call < 2000; call++) {
    total += left(changed) + right(changed);
    retrained += leaf(changed);
}
JSON.stringify([direct, warmed, first, total, retrained]);
"#;
    let run = compare_with_entry_probes(
        SETUP,
        SOURCE,
        "[4000,14000,6,30000,12000]",
        &[
            entry_probe("left", "String(left(original));", "3"),
            entry_probe("right", "String(right(original));", "4"),
        ],
        &[entry_probe("leaf", "String(leaf(changed));", "6")],
    );
    for caller in ["left", "right"] {
        run.assert_optimized(caller);
        let generation = run
            .entry_probes
            .iter()
            .find_map(|(name, id)| (name == caller).then_some(*id))
            .expect("caller warmup must enter its exact installed generation");
        run.assert_spliced(caller, "leaf", Some(generation));
        assert!(
            run.optimizing_exits(caller).is_empty(),
            "invalidated inline callers must not enter stale shape guards: {:?}",
            run.optimizing_exits(caller)
        );
    }
    let exits = run.optimizing_exits("leaf");
    assert_eq!(exits.len(), 1, "the callee must retrain once: {exits:?}");
    assert_eq!(exits[0].1, ExitReason::ShapeGuard);
    assert!(run.stats.jit_caller_invalidations >= 2, "{:?}", run.stats);
    run.assert_recompiled_after_last_exit("leaf");
}

#[test]
fn active_looping_inline_caller_leaves_invalidated_generation_at_poll() {
    const SETUP: &str = r#"
function leaf(record) { return record.value + 1; }
function trigger(record) { return leaf(record) + arguments.length; }
function drive(record, count, callbackRecord) {
    let total = 0;
    for (let index = 0; index < count; index++) {
        if (index === 64) trigger(callbackRecord);
        total += leaf(record);
    }
    return total + arguments.length;
}
const original = { value: 1 };
for (let call = 0; call < 2000; call++) leaf(original);
for (let call = 0; call < 2000; call++) trigger(original);
let warmed = 0;
for (let call = 0; call < 2000; call++) warmed += drive(original, 128, original);
"#;
    const SOURCE: &str = r#"
const total = drive(original, 20000, { extra: 0, value: 5 });
JSON.stringify([warmed, total]);
"#;
    let run = compare_with_setup(SETUP, SOURCE, "[518000,40003]");
    run.assert_optimized("drive");
    run.assert_spliced("drive", "leaf", None);
    // A callee's feedback change discards no installed caller: the active
    // generation leaves through its own guard on the new shape (or, when a
    // dependency invalidated it, at its bounded loop poll), then recompiles.
    let exits = run.optimizing_exits("drive");
    assert!(
        !exits.is_empty()
            && exits.iter().all(|(_, reason)| matches!(
                reason,
                ExitReason::ShapeGuard | ExitReason::Interrupt
            )),
        "the active generation must leave at its guard or bounded loop poll: {exits:?}"
    );
    run.assert_recompiled_after_last_exit("drive");
    assert!(
        run.osr_entries_after_setup
            .is_some_and(|entries| entries > 0),
        "the sole final loop must actually enter replacement optimizing OSR: {:?}",
        run.stats
    );
}
