//! Template-tier parity for synchronous module bytecodes.
//!
//! # Contents
//! - Eager namespace and live named-import reads through a linked graph.
//! - Star re-export and deferred namespace creation/forced evaluation.
//! - `import.meta.resolve` plus observable module-body side effects.
//!
//! # Invariants
//! - Template output is byte-identical to the interpreter oracle.
//! - A source-identified Template generation survives full GC and a live-export
//!   mutation; the measured call has no interpreted instructions in that body.
//! - Native coverage is for `LoadImportBinding` in `hot`; one-shot namespace,
//!   re-export, deferred evaluation and import-meta setup retain semantic checks.
//! - The fixture enters the shared reentrant module transition.
//! - Promise-producing dynamic import and top-level-await evaluation are not
//!   part of this synchronous fixture.

use std::sync::{Arc, Mutex};

use otter_bytecode::Op;
use otter_runtime::{
    JitSelection, Runtime, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::native_abi::{CodeLifetimeState, NativeFrameKind};

#[derive(Default)]
struct HotDispatches {
    function_id: Option<u32>,
    steps: usize,
    imports: usize,
}

struct Tracer(Arc<Mutex<HotDispatches>>);

impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        if event.function_name == "hot" {
            let mut trace = self.0.lock().unwrap();
            trace.function_id = Some(event.function_id);
            trace.steps += 1;
            trace.imports += usize::from(event.op == Op::LoadImportBinding);
        }
    }
}

fn run(selection: JitSelection) -> String {
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

globalThis.moduleHot = hot;
globalThis.moduleBump = bump;
const resolved = import.meta.resolve("./dep.mjs");
const before = globalThis.moduleEffects;
const bumped = bump();
const lazyValue = deferred.lazy;
globalThis.moduleBefore = [
  before,
  bumped,
  lazyValue,
  hot(1000), // Earn normal source-work admission, not a test-only tier threshold.
  globalThis.moduleEffects,
  resolved.endsWith("/dep.mjs")
];
"#,
    )
    .expect("write entry");

    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(selection)
        .build()
        .expect("runtime");
    let trace = Arc::new(Mutex::new(HotDispatches::default()));
    runtime.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    runtime.run_module(&entry).expect("run module graph");
    let function_id = trace
        .lock()
        .unwrap()
        .function_id
        .expect("observed hot source");
    let generation = if matches!(selection, JitSelection::Template) {
        Some(
            runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .find(|g| {
                    g.function_id == function_id
                        && g.tier == NativeFrameKind::Baseline
                        && g.lifecycle == CodeLifetimeState::Installed
                        && g.current_entry
                        && g.linked
                })
                .expect("normal policy must install the exact hot body"),
        )
    } else {
        None
    };
    runtime
        .force_gc()
        .expect("collect before reusing module generation");
    *trace.lock().unwrap() = HotDispatches::default();
    let reentrant = runtime.execution_stats().jit_reentrant_stub_transitions;
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(
                "JSON.stringify([moduleBefore, moduleBump(), moduleHot(80), moduleEffects]);",
            ),
            "module-live-read.js",
        )
        .expect("read changed export from installed code")
        .completion_string()
        .to_owned();
    let observed = trace.lock().unwrap();
    if let Some(generation) = generation {
        assert_eq!(
            observed.steps, 0,
            "hot must not fall back to interpreter dispatch"
        );
        assert!(
            runtime.execution_stats().jit_reentrant_stub_transitions > reentrant,
            "measured module reads must enter the shared reentrant transition"
        );
        assert!(
            runtime.jit_code_generation_snapshot().iter().any(|g| {
                g.code_object_id == generation.code_object_id
                    && g.function_id == function_id
                    && g.tier == NativeFrameKind::Baseline
                    && g.lifecycle == CodeLifetimeState::Installed
                    && g.current_entry
                    && g.linked
            }),
            "reuse the same native generation after collection and export mutation"
        );
    } else {
        assert!(
            observed.imports > 0,
            "oracle must execute LoadImportBinding in hot"
        );
    }
    completion
}

#[test]
fn synchronous_module_ops_match_interpreter_with_observable_effects() {
    let oracle = run(JitSelection::InterpreterOnly);
    let compiled = run(JitSelection::Template);
    assert_eq!(compiled, oracle);
    assert_eq!(compiled, "[[1,41,5,45000,14,true],42,3680,17]");
}
