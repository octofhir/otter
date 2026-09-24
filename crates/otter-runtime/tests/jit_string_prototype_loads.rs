//! Generated named loads from `%String.prototype%` on primitive receivers.
//!
//! # Contents
//! - Warm `s.charCodeAt` / `s.indexOf` loads complete without a property
//!   transition in every tier that generates them.
//! - Same-slot value replacement is read live; additions, deletions and
//!   accessor redefinitions retire the dictionary layout proof.
//! - A primitive string's own `length` and index keys never read the
//!   prototype, even when the prototype defines them.
//! - Moving collection between hits preserves receivers and loaded values.
//!
//! # Invariants
//! - A proof miss commits the canonical property load once.
//! - Every in-place descriptor change of a dictionary object assigns a fresh
//!   structural id, so a stale slot proof can never read an accessor cell.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const TIERS: [JitSelection; 3] = [
    JitSelection::InterpreterOnly,
    JitSelection::Template,
    JitSelection::ProductionTiered,
];

const SETUP: &str = r#"
const originalCharCodeAt = String.prototype.charCodeAt;
function loadCode(text, count) {
    let last;
    for (let index = 0; index < count; index++) last = text.charCodeAt;
    return last;
}
function sameCode(text, count) {
    let same = 0;
    for (let index = 0; index < count; index++) {
        if (text.charCodeAt === originalCharCodeAt) same++;
    }
    return same;
}
function loadNamed(text, count) {
    let last;
    for (let index = 0; index < count; index++) last = text.probe;
    return last;
}
function loadIndex(text, count) {
    let last;
    for (let index = 0; index < count; index++) last = text[1] + text.length;
    return last;
}
for (let warm = 0; warm < 40; warm++) {
    loadCode("otter", 200);
    sameCode("otter", 200);
    loadIndex("otter", 200);
}
"#;

fn warmed(selection: JitSelection) -> Runtime {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .build()
        .expect("string prototype load runtime");
    completion(&mut runtime, SETUP);
    runtime
}

fn completion(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), "string-loads.js")
        .unwrap_or_else(|error| panic!("string prototype load fixture: {error:?}"))
        .completion_string()
        .to_owned()
}

#[test]
fn warm_prototype_loads_do_not_enter_the_property_boundary() {
    for selection in TIERS {
        let mut runtime = warmed(selection);
        let before = runtime.execution_stats();
        assert_eq!(completion(&mut runtime, "sameCode('otter', 1000)"), "1000");
        let after = runtime.execution_stats();
        assert_eq!(after.jit_optimized_deopts, before.jit_optimized_deopts);
        if selection != JitSelection::InterpreterOnly {
            assert_eq!(
                after.jit_runtime_property_stubs - before.jit_runtime_property_stubs,
                0,
                "{selection:?}: every warm charCodeAt load must stay generated"
            );
        }
    }
}

#[test]
fn layout_changes_and_own_keys_match_the_interpreter() {
    const CHANGES: &str = r#"
const log = [];
const replacement = function () { return 1; };
String.prototype.charCodeAt = replacement;
log.push(loadCode("otter", 3) === replacement);
String.prototype.charCodeAt = originalCharCodeAt;
String.prototype.probe = 5;
log.push(loadNamed("otter", 3));
delete String.prototype.probe;
log.push(loadNamed("otter", 3));
let getterReads = 0;
Object.defineProperty(String.prototype, "charCodeAt", {
    configurable: true,
    get() { getterReads++; return replacement; }
});
log.push(loadCode("otter", 4) === replacement, getterReads);
Object.defineProperty(String.prototype, "charCodeAt", {
    configurable: true, writable: true, enumerable: false, value: originalCharCodeAt
});
log.push(sameCode("otter", 4));
String.prototype[1] = "prototype";
String.prototype.length = 99;
log.push(loadIndex("otter", 3), loadIndex("x", 2));
const names = Object.getOwnPropertyNames(String.prototype);
const next = names[names.indexOf("charCodeAt") + 1];
String.prototype[next] = originalCharCodeAt;
delete String.prototype.charCodeAt;
log.push(loadCode("otter", 2) === undefined);
JSON.stringify(log);
"#;
    let mut oracle = warmed(JitSelection::InterpreterOnly);
    let expected = completion(&mut oracle, CHANGES);
    assert_eq!(expected, r#"[true,5,null,true,4,4,"t5","prototype1",true]"#);
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = warmed(selection);
        assert_eq!(completion(&mut runtime, CHANGES), expected, "{selection:?}");
    }
}

#[test]
fn moving_collection_between_loads_preserves_receivers() {
    const CHURN: &str = r#"
function churn(count) {
    const kept = [];
    let same = 0;
    for (let index = 0; index < count; index++) {
        const word = "engine-" + (index * 7919 + 100003);
        kept.push(word);
        if (kept[index >> 1].indexOf === String.prototype.indexOf) same++;
        if (word.charCodeAt === originalCharCodeAt) same++;
    }
    return same;
}
for (let warm = 0; warm < 40; warm++) churn(64);
churn(512);
"#;
    for selection in TIERS {
        let mut runtime = warmed(selection);
        let before = runtime.execution_stats();
        assert_eq!(completion(&mut runtime, CHURN), "1024", "{selection:?}");
        let after = runtime.execution_stats();
        if let Some(stride @ (1 | 4 | 16)) = std::env::var("OTTER_GC_STRESS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            assert!(after.gc_minor_cycles - before.gc_minor_cycles >= 128 / stride);
        }
    }
}
