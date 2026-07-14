//! Template-tier parity for synchronous module bytecodes.
//!
//! # Contents
//! - Eager namespace and live named-import reads through a linked graph.
//! - Star re-export and deferred namespace creation/forced evaluation.
//! - `import.meta.resolve` plus observable module-body side effects.
//!
//! # Invariants
//! - Template output is byte-identical to the interpreter oracle.
//! - The fixture enters the shared reentrant module transition.
//! - Promise-producing dynamic import and top-level-await evaluation are not
//!   part of this synchronous fixture.

use std::sync::{Arc, Mutex};

use otter_runtime::{ConsoleLevel, ConsoleSink, JitSelection, Runtime};

#[derive(Debug, Default)]
struct LogCapture {
    lines: Mutex<Vec<String>>,
}

impl ConsoleSink for LogCapture {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        if matches!(level, ConsoleLevel::Log) {
            self.lines
                .lock()
                .expect("log capture")
                .push(fields.join(" "));
        }
    }
}

fn run(selection: JitSelection) -> (Vec<String>, u64) {
    let dir = tempfile::tempdir().expect("module tempdir");
    std::fs::write(
        dir.path().join("dep.mjs"),
        r#"
globalThis.moduleEffects = (globalThis.moduleEffects ?? 0) + 1;
export let live = 40;
export const extra = 2;
export function bump() {
  globalThis.moduleEffects += 3;
  live += 1;
  return live;
}
"#,
    )
    .expect("write dep");
    std::fs::write(dir.path().join("star.mjs"), r#"export * from "./dep.mjs";"#)
        .expect("write star");
    std::fs::write(
        dir.path().join("lazy.mjs"),
        r#"
globalThis.moduleEffects += 10;
export const lazy = 5;
"#,
    )
    .expect("write lazy");
    let entry = dir.path().join("entry.mjs");
    std::fs::write(
        &entry,
        r#"
import { live, bump } from "./dep.mjs";
import * as ns from "./dep.mjs";
import { extra } from "./star.mjs";
import defer * as deferred from "./lazy.mjs";

function hot(rounds) {
  let total = 0;
  for (let i = 0; i < rounds; i++) {
    total += live + extra + ns.extra;
  }
  return total;
}

const resolved = import.meta.resolve("./dep.mjs");
const before = globalThis.moduleEffects;
const bumped = bump();
const lazyValue = deferred.lazy;
console.log(JSON.stringify([
  before,
  bumped,
  lazyValue,
  hot(80),
  globalThis.moduleEffects,
  resolved.endsWith("/dep.mjs")
]));
"#,
    )
    .expect("write entry");

    let capture = Arc::new(LogCapture::default());
    let mut runtime = Runtime::builder()
        .console_sink(capture.clone())
        .jit_selection(selection)
        .jit_osr_threshold(1)
        .build()
        .expect("runtime");
    runtime.run_module(&entry).expect("run module graph");
    let reentrant = runtime.execution_stats().jit_reentrant_stub_transitions;
    let lines = capture.lines.lock().expect("log capture").clone();
    (lines, reentrant)
}

#[test]
fn synchronous_module_ops_match_interpreter_with_observable_effects() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, reentrant) = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert_eq!(compiled, vec!["[1,41,5,3600,14,true]"]);
    assert!(
        reentrant > 0,
        "module fixture must enter a shared reentrant transition"
    );
}
