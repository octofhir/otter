//! Generated dependencies on complete prototype chains.
//!
//! # Contents
//! Deep inherited loads emit validity cells and observe prototype mutations.
//! Inlined method misses retain their source call, and the mutation corpus
//! completes through every execution tier. Constructor closures keep distinct
//! prototype proofs while empty receiver allocation uses each live root.
//! Restored isolates rebuild feedback and chain dependencies independently.
//!
//! # Invariants
//! Both generated tiers use a cell even when the holder is beyond eight hops.
//! Invalidated generations never hide a new value or shadowing property.
//!
//! # See also
//! `otter_vm::object` owns chain dependencies and mutation invalidation.

use otter_runtime::{
    JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime, SourceInput,
};

#[test]
fn deep_prototype_load_uses_validity_cell() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts())
            .build()
            .unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
const root = {value: 7};
let chain = root;
for (let depth = 0; depth < 12; depth++) chain = Object.create(chain);
const receiver = Object.create(chain);
function readDeep(receiver) { return receiver.value; }
let sum = 0;
for (let warm = 0; warm < 5000; warm++) sum += readDeep(receiver);
sum
"#,
                ),
                "deep-prototype-validity.js",
            )
            .unwrap();
        assert_eq!(result.completion_string(), "35000");
        assert!(
            result
                .jit_artifacts()
                .unwrap()
                .bundles()
                .iter()
                .any(|bundle| {
                    bundle.manifest().function_name() == "readDeep"
                        && std::str::from_utf8(
                            bundle
                                .file(JitArtifactFileName::Relocations)
                                .unwrap()
                                .contents(),
                        )
                        .unwrap()
                        .contains("prototypeValidityCell")
                }),
            "{selection:?}: the deep load must generate a validity-cell guard"
        );
        if selection == JitSelection::ProductionTiered {
            assert!(
                result
                    .jit_artifacts()
                    .unwrap()
                    .bundles()
                    .iter()
                    .any(|bundle| {
                        bundle.manifest().tier() == JitDebugTier::Optimizing
                            && matches!(bundle.manifest().function_name(), "readDeep" | "<main>")
                            && std::str::from_utf8(
                                bundle
                                    .file(JitArtifactFileName::Relocations)
                                    .unwrap()
                                    .contents(),
                            )
                            .unwrap()
                            .contains("prototypeValidityCell")
                    }),
                "the optimizing deep load must retain the chain proof"
            );
        }
        let result = runtime
            .run_script(
                SourceInput::from_javascript(
                    "root.value = 19; const changed = readDeep(receiver); chain.value = 23; changed + readDeep(receiver)",
                ),
                "mutate-deep-prototype.js",
            )
            .unwrap();
        assert_eq!(result.completion_string(), "42");
    }
}

#[test]
fn inlined_deep_method_miss_uses_its_source_call() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
function read(o) { return o.x; }
function call(o) { return o.f(); }
function warm(o) {
    let sum = 0;
    for (let i = 0; i < 1200; i++) sum += read(o) + call(o);
    return sum;
}

const root = {x: 2, f() { return this.x + 1; }};
let chain = root;
for (let i = 0; i < 12; i++) chain = Object.create(chain);
const a = Object.create(chain);
const b = Object.create(chain);
if (warm(a) !== 6000 || warm(b) !== 6000) throw new Error('warm');
root.x = 4;
if (read(a) !== 4 || call(b) !== 5) throw new Error('value mutation');
root.f = function () { return this.x + 10; };
warm(a)
"#,
                ),
                "inlined-deep-method.js",
            )
            .unwrap();
        assert_eq!(result.completion_string(), "21600", "{selection:?}");
    }
}

#[test]
fn prototype_validity_corpus_completes_in_every_tier() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../otter-difftest/corpus/prototype_validity_cells.js"
                )),
                "prototype-validity-corpus.js",
            )
            .unwrap_or_else(|error| panic!("{selection:?}: {error:?}"));
    }
}

#[test]
fn constructor_closures_allocate_on_their_live_prototype_roots() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .build()
        .unwrap();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function makeConstructor(kind) {
    function C(value) {
        if (value >= 0) this.value = value;
        this.kind = kind;
    }
    C.prototype = { inherited: kind };
    return C;
}

const constructors = [];
for (let i = 0; i < 12; i++) constructors.push(makeConstructor(i));
function constructMany() {
    let sum = 0;
    for (let i = 0; i < 2400; i++) {
        const C = constructors[i % constructors.length];
        const object = new C(i);
        if (Object.getPrototypeOf(object) !== C.prototype) throw new Error('root');
        if (object.kind !== object.inherited) throw new Error('lineage');
        sum += object.value;
    }
    return sum;
}
constructMany();
constructMany()
"#,
            ),
            "constructor-prototype-roots.js",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "2878800");
    let stats = runtime.execution_stats();
    assert!(stats.jit_receiver_alloc_attempts > 0);
    if std::env::var_os("OTTER_GC_STRESS").is_none() {
        assert!(
            stats.jit_receiver_alloc_generated > stats.jit_receiver_alloc_guard_misses,
            "ordinary allocation must accept different live prototype roots: {stats:?}"
        );
    }
}

#[test]
fn restored_code_does_not_retain_donor_property_feedback() {
    let snapshot = {
        let donor = Runtime::builder()
            .extension_installer(otter_runtime::RuntimeExtensionInstaller::new(|ctx| {
                ctx.install_script(SourceInput::from_javascript(
                    r#"
globalThis.snapshotPrototype = {value: 7};
globalThis.snapshotReceiver = Object.create(snapshotPrototype);
globalThis.snapshotRead = function (object) { return object.value; };
globalThis.snapshotWrite = function (object, value) { object.own = value; return object.own; };
for (let i = 0; i < 1000; i++) {
    snapshotRead(snapshotReceiver);
    snapshotWrite(Object.create(snapshotPrototype), i);
}
"#,
                ))
            }))
            .build()
            .unwrap();
        donor.capture_isolate_snapshot().unwrap()
    };
    let mut first = Runtime::from_isolate_snapshot(&snapshot).unwrap();
    let mut second = Runtime::from_isolate_snapshot(&snapshot).unwrap();
    first
        .eval(SourceInput::from_javascript(
            "snapshotPrototype.value = 19; snapshotWrite(snapshotReceiver, 31)",
        ))
        .unwrap();
    second
        .eval(SourceInput::from_javascript(
            "for (let i = 0; i < 3000; i++) snapshotWrite(Object.create(snapshotPrototype), i)",
        ))
        .unwrap();
    drop(first);
    second.force_gc().unwrap();
    let result = second
        .eval(SourceInput::from_javascript(
            "snapshotRead(snapshotReceiver) + ':' + snapshotWrite({}, 23)",
        ))
        .unwrap();
    assert_eq!(result.completion_string(), "7:23");
}
