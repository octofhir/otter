//! Generated `instanceof` over ordinary closure targets.
//!
//! # Contents
//! - A warm `value instanceof F` site answers ordinary objects, primitives
//!   and `null` inside the Machine probe without the object-protocol boundary.
//! - Replacing `F.prototype`, reparenting `F`, an own `Symbol.hasInstance`,
//!   Proxies, bound and class targets, null-prototype objects, deep chains and
//!   a non-object `prototype` all match the interpreter in every tier.
//!
//! # Invariants
//! - A proof miss commits one canonical `instanceof`; no tier deopts.

use otter_runtime::{
    ExecutionResult, JitArtifactFileName, JitDebugRequest, JitDebugTier, JitSelection, Runtime,
    SourceInput,
};

const TIERS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

const SETUP: &str = r#"
function Pair(car, cdr) { this.car = car; this.cdr = cdr; }
function Leaf() {}
Leaf.prototype = Object.create(Pair.prototype);
function isPair(value, target) { return value instanceof target; }
function countPairs(values, count) {
    let pairs = 0;
    for (let index = 0; index < count; index++) {
        if (values[index % values.length] instanceof Pair) pairs++;
    }
    return pairs;
}
globalThis.mixed = [new Pair(1, 2), "text", 7, null, new Leaf(), {}, undefined, 1.5];
for (let warm = 0; warm < 60; warm++) {
    countPairs(mixed, 400);
    isPair(new Pair(1, 2), Pair);
    isPair("text", Pair);
}
"#;

fn runtime(selection: JitSelection) -> Runtime {
    Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("instanceof runtime")
}

fn run(runtime: &mut Runtime, source: &str) -> ExecutionResult {
    runtime
        .run_script(SourceInput::from_javascript(source), "instanceof.js")
        .unwrap_or_else(|error| panic!("instanceof fixture: {error:?}"))
}

fn completion(runtime: &mut Runtime, source: &str) -> String {
    run(runtime, source).completion_string().to_owned()
}

fn warmed(selection: JitSelection) -> Runtime {
    let mut runtime = runtime(selection);
    let result = run(&mut runtime, SETUP);
    if selection == JitSelection::ProductionTiered {
        let bundle = result
            .jit_artifacts()
            .expect("instanceof artifacts")
            .bundles()
            .iter()
            .find(|bundle| {
                bundle.manifest().function_name() == "countPairs"
                    && bundle.manifest().tier() == JitDebugTier::Optimizing
            })
            .unwrap_or_else(|| panic!("countPairs must optimize: {:?}", result.jit_debug_report()));
        let code_map = std::str::from_utf8(
            bundle
                .file(JitArtifactFileName::CodeMap)
                .expect("instanceof code map")
                .contents(),
        )
        .expect("UTF-8 code map");
        assert!(
            code_map.contains("machineInstanceofProbe"),
            "countPairs lacks the instanceof probe: {code_map}"
        );
    }
    runtime
}

#[test]
fn warm_instanceof_stays_in_the_generated_probe() {
    let mut runtime = warmed(JitSelection::ProductionTiered);
    let before = runtime.execution_stats();
    assert_eq!(completion(&mut runtime, "countPairs(mixed, 8000)"), "2000");
    let after = runtime.execution_stats();
    assert!(after.jit_optimized_entries > before.jit_optimized_entries);
    assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
    assert!(
        after.jit_reentrant_stub_transitions - before.jit_reentrant_stub_transitions < 16,
        "warm instanceof must not reach the object-protocol stub: {}",
        after.jit_reentrant_stub_transitions - before.jit_reentrant_stub_transitions
    );
}

#[test]
fn instanceof_edge_cases_match_the_interpreter() {
    const PROBES: &str = r#"
const results = [];
const pair = new Pair(1, 2);
results.push(isPair(pair, Pair), isPair(new Leaf(), Pair), isPair({}, Pair));
results.push(isPair(Object.create(null), Pair), isPair(Symbol("s"), Pair), isPair(10n, Pair));
results.push(isPair(new Proxy(pair, {}), Pair), isPair([], Array), isPair(isPair, Function));
const Bound = Pair.bind(null);
results.push(isPair(pair, Bound));
class Cell {}
results.push(isPair(new Cell(), Cell), isPair(pair, Cell));
let deep = pair;
for (let depth = 0; depth < 40; depth++) deep = Object.create(deep);
results.push(isPair(deep, Pair));
function Custom() {}
Object.defineProperty(Custom, Symbol.hasInstance, { value: (v) => v === 7 });
results.push(isPair(7, Custom), isPair(new Custom(), Custom));
function Reparented() {}
Object.setPrototypeOf(Reparented, { [Symbol.hasInstance]: () => true });
results.push(isPair({}, Reparented));
const oldPrototype = Pair.prototype;
Pair.prototype = {};
results.push(isPair(pair, Pair), isPair(Object.create(Pair.prototype), Pair));
Pair.prototype = 5;
try { isPair(pair, Pair); results.push("no throw"); } catch (error) { results.push(error.name); }
Pair.prototype = oldPrototype;
try { isPair(pair, {}); results.push("no throw"); } catch (error) { results.push(error.name); }
results.push(countPairs(mixed, 800));
JSON.stringify(results);
"#;
    let mut expected = None;
    for selection in TIERS {
        let mut runtime = warmed(selection);
        let actual = completion(&mut runtime, PROBES);
        match &expected {
            None => expected = Some(actual),
            Some(expected) => assert_eq!(&actual, expected, "{selection:?}"),
        }
    }
    assert_eq!(
        expected.as_deref(),
        Some(
            r#"[true,true,false,false,false,false,true,true,true,true,true,false,true,true,false,true,false,true,"TypeError","TypeError",200]"#
        )
    );
}
