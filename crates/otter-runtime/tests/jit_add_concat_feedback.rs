//! Numeric Add after string-concatenation feedback through the public runtime.
//!
//! # Contents
//! - A shared differential regression for an inlined conditional operand.
//! - Normal-policy optimizing admission before checking the numeric result.
//! - Non-String misses preserve primitive conversion, errors and UTF-16 units.
//!
//! # Invariants
//! - Historical string feedback cannot force current numeric operands to strings.
//! - Interpreter, Template and production policies retain identical JS results.
//!
//! # See also
//! - `otter-jit/src/graph/builder.rs` owns speculative Add construction.

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn concat_feedback_does_not_change_later_numeric_addition() {
    let source = include_str!("../../otter-difftest/corpus/add_concat_feedback.js");
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_compile_threads(0)
            .jit_selection(selection)
            .build()
            .expect("Add feedback runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source),
                "add-concat-feedback.js",
            )
            .expect("Add feedback program");
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        if selection == JitSelection::ProductionTiered {
            assert!(
                runtime.execution_stats().jit_optimized_entries > 0,
                "regression must execute optimizing code under normal admission"
            );
        }
        assert_eq!(
            result.completion_string(),
            r#"[800020000,["2!",3.5,8],"number",true]"#,
            "{selection:?}"
        );
    }
}

#[test]
fn concat_guard_misses_preserve_coercion_order_and_errors() {
    let source = r#"
function leftNumber(n, other) { return (n & -1) + other; }
function rightNumber(other, n) { return other + (n & -1); }
for (let warm = 0; warm < 6000; warm++) {
    leftNumber(warm & 15, "!");
    rightNumber("!", warm & 15);
}
const log = [];
const marker = {};
function argument(value) { log.push("argument"); return value; }
const numericObject = {
    [Symbol.toPrimitive](hint) { log.push(hint); return 9; }
};
const throwingObject = {
    get [Symbol.toPrimitive]() {
        log.push("get");
        return function(hint) { log.push(hint); throw marker; };
    }
};
const objectSums = [
    leftNumber(41, argument(numericObject)),
    rightNumber(argument(numericObject), 41)
];
let throws = 0;
for (const operation of [
    () => leftNumber(41, argument(throwingObject)),
    () => rightNumber(argument(throwingObject), 41)
]) {
    try { operation(); } catch (error) { if (error === marker) throws++; }
}
let typeErrors = 0;
for (const other of [1n, Symbol("s")]) {
    for (const operation of [() => leftNumber(41, other), () => rightNumber(other, 41)]) {
        try { operation(); } catch (error) { if (error instanceof TypeError) typeErrors++; }
    }
}
const high = leftNumber(7, "\ud800");
const low = rightNumber("\udc00", 7);
JSON.stringify([
    [leftNumber(41, 1), rightNumber(1, 41), leftNumber(41, true),
     rightNumber(null, 41), Number.isNaN(leftNumber(41, undefined)),
     Number.isNaN(rightNumber(undefined, 41)), leftNumber(41, 1.5),
     Object.is(leftNumber(0, -0), 0)],
    objectSums, throws, typeErrors, log,
    [high.length, high.charCodeAt(0), high.charCodeAt(1)],
    [low.length, low.charCodeAt(0), low.charCodeAt(1)]
]);
"#;
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_compile_threads(0)
            .jit_selection(selection)
            .build()
            .expect("Add coercion runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source),
                "add-concat-coercion.js",
            )
            .expect("catchable conversion errors");
        assert_eq!(
            result.completion_string(),
            r#"[[42,42,42,41,true,true,42.5,true],[50,50],2,4,["argument","default","argument","default","argument","get","default","argument","get","default"],[2,55,55296],[2,56320,55]]"#,
            "{selection:?}"
        );
    }
}
