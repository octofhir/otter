//! Committed generic binary operators in the optimizing tier.
//!
//! # Contents
//! - Relational and additive sites whose feedback saw a non-Number operand
//!   keep the function optimized; Number operands complete without a runtime
//!   transition.
//! - Object operands coerce exactly once, left before right, including a
//!   throwing `valueOf` caught by the caller.
//! - Strings, BigInt, Symbol, `%`, `**`, NaN and negative zero keep exact
//!   ECMAScript results.
//!
//! # Invariants
//! - Results match the interpreter oracle exactly.
//! - A generic site never deoptimizes: every non-Number operand completes
//!   once through the committed operator. A site first compiled from Number
//!   feedback takes at most one exact exit before it turns generic.
//! - A script body compiled on a loop header leaves once at the first site
//!   after the loop that never ran before the compile.

use otter_runtime::{
    JitDebugEvent, JitDebugRequest, JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};
use otter_vm::native_abi::ExitReason;

struct Run {
    completion: String,
    stats: RuntimeExecutionStats,
    /// Function name and reason of every side exit taken by generated code.
    exits: Vec<(String, ExitReason)>,
}

fn run(source: &str, selection: JitSelection) -> Run {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("generic operator runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(source), "generic-binary.js")
        .unwrap_or_else(|error| panic!("generic operator fixture: {error:?}"));
    let exits = result
        .jit_debug_report()
        .expect("events enabled")
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::Bail {
                function_name,
                exit_reason,
                ..
            } => Some((function_name.clone(), *exit_reason)),
            _ => None,
        })
        .collect();
    Run {
        completion: result.completion_string().to_owned(),
        stats: runtime.execution_stats(),
        exits,
    }
}

fn compare(source: &str, expected: &str) -> Run {
    let oracle = run(source, JitSelection::InterpreterOnly);
    assert_eq!(oracle.completion, expected);
    let compiled = run(source, JitSelection::ProductionTiered);
    assert_eq!(compiled.completion, oracle.completion);
    compiled
}

#[test]
fn polluted_sites_stay_optimized_and_numbers_stay_inline() {
    const SOURCE: &str = r#"
function polluted(count) {
    let checksum = 0;
    for (let index = 0; index < count; index++) {
        const lane = index & 15;
        const threshold = index === 0 ? undefined : 8;
        const offset = index === 0 ? undefined : 3;
        if (lane > threshold) checksum = checksum + 1;
        const shifted = lane + offset;
        if (shifted === shifted) checksum = checksum + shifted * 2;
    }
    return checksum;
}
let total = 0;
for (let call = 0; call < 40; call++) total += polluted(5000);
String(total);
"#;
    let run = compare(SOURCE, "4284560");
    assert_eq!(run.stats.jit_optimized_deopts, 0, "{:?}", run.stats);
    assert!(
        run.stats.jit_reentrant_stub_transitions <= 2 * 40,
        "Number operands must complete without a runtime transition: {:?}",
        run.stats
    );
}

#[test]
fn object_operands_coerce_once_left_before_right() {
    const SOURCE: &str = r#"
const log = [];
function tagged(name, value) {
    return { valueOf() { log.push(name); return value; } };
}
function mix(a, b) { return [a + b, a - b, a * b, a / b, a % b, a ** b, a < b, a <= b, a > b, a >= b]; }
let last;
// Every hundredth call passes a String, so the sites turn generic.
for (let call = 0; call < 3000; call++) last = mix(call & 7, call % 100 === 99 ? "2" : 2);
log.length = 0;
const result = mix(tagged("L", 9), tagged("R", 2));
let caught = "";
try {
    mix({ valueOf() { throw new Error("boom"); } }, tagged("never", 1));
} catch (error) {
    caught = error.message;
}
JSON.stringify([last, result, log.join(""), caught]);
"#;
    let run = compare(
        SOURCE,
        r#"[["72",5,14,3.5,1,49,false,false,true,true],[11,7,18,4.5,1,81,false,false,true,true],"LRLRLRLRLRLRLRLRLRLR","boom"]"#,
    );
    // At most one exact exit in `mix` when object operands first reach its
    // numeric sites, plus one in the OSR-compiled script body when it first
    // calls the never-called `tagged`.
    assert!(run.stats.jit_optimized_deopts <= 2, "{:?}", run.stats);
}

#[test]
fn strings_bigint_symbol_nan_and_negative_zero_stay_exact() {
    const SOURCE: &str = r#"
function ops(a, b) {
    return [a + b, a - b, a * b, a < b, a >= b];
}
const cases = [
    [1, "2"], ["a", 3], [5n, 2n], [NaN, 1], [1, NaN], [-0, 0], [0, -1], [undefined, 1], [null, 2],
    [2147483647, 2], [-2147483648, 1], [65536, 65536], [7, -3],
];
// Every hundredth call passes a String, so the sites turn generic.
for (let warm = 0; warm < 3000; warm++) ops(warm & 3, warm % 100 === 99 ? "1" : 1);
const out = [];
for (let round = 0; round < 50; round++) {
    for (const [a, b] of cases) out.push(ops(a, b).map((value) => typeof value === "bigint" ? value + "n" : Object.is(value, -0) ? "-0" : value));
}
let symbolError = "";
try { ops(Symbol("s"), 1); } catch (error) { symbolError = error.name; }
let mixedError = "";
try { ops(1n, 1); } catch (error) { mixedError = error.name; }
JSON.stringify([out.slice(0, cases.length), out.length, symbolError, mixedError]);
"#;
    let run = compare(
        SOURCE,
        r#"[[["12",-1,2,true,false],["a3",null,null,false,false],["7n","3n","10n",false,true],[null,null,null,false,false],[null,null,null,false,false],[0,"-0","-0",false,true],[-1,1,"-0",false,true],[null,null,null,false,false],[2,-2,0,true,false],[2147483649,2147483645,4294967294,false,true],[-2147483647,-2147483649,-2147483648,true,false],[131072,0,4294967296,false,true],[4,10,-21,false,true]],650,"TypeError","TypeError"]"#,
    );
    // `ops` saw strings while warming, so its sites are generic and never
    // exit. The script body is compiled on each of its two loop headers and
    // leaves each generation once, at the first never-run site after the loop.
    assert!(
        run.exits
            .iter()
            .all(|(name, reason)| name == "<main>" && *reason == ExitReason::InsufficientFeedback),
        "{:?}",
        run.exits
    );
    assert!(run.stats.jit_optimized_deopts <= 2, "{:?}", run.stats);
}
