//! Optimizing on-stack replacement and post-entry deoptimization parity.
//!
//! # Contents
//! - A once-called float array read-modify-write loop that can only reach the
//!   optimizing tier through a hot back-edge.
//! - An int32 loop that overflows after optimized OSR and resumes from its
//!   reconstructed interpreter frame.
//! - A once-called loop reading one invariant property, whose access the
//!   optimizing tier moves into a pre-header the OSR entry never reaches.
//!
//! # Invariants
//! - Tiered and interpreter-only runs execute identical source.
//! - Setup runs before the measured probe; only the named subject can loop
//!   during the probe and claim its optimizing OSR entry.
//! - Every native entry joins to that subject's exact module, function, OSR
//!   header, executable bytes, and Graph instruction regions on both targets.
//! - A post-OSR deopt preserves all loop-carried values needed for completion.
//! - A loop whose invariant access was hoisted still has an OSR entry, which
//!   performs that access itself before entering the loop.

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest,
    JitDebugTarget, JitDebugTier, JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};
use otter_vm::native_abi::{ExitAction, ExitReason};

const ARRAY_RMW_SETUP: &str = r#"
    const osrA = [];
    const osrB = [];
    for (let setup = 0; setup < 8192; setup = setup + 1) {
      osrA[setup] = 1.25;
      osrB[setup] = 0.5;
    }

    function onceFloatRmw(a, b, scale, limit) {
      let total = 0.5;
      for (let index = 0; index < limit; index = index + 1) {
        a[index] = a[index] * scale + b[index];
        total = total + a[index];
      }
      return total;
    }

"#;

const ARRAY_RMW_PROBE: &str = r#"
    const osrRmwResult = onceFloatRmw(osrA, osrB, 0.5, 8192);
    JSON.stringify({ result: osrRmwResult, first: osrA[0], last: osrA[8191] });
"#;

const POST_OSR_DEOPT_SETUP: &str = r#"
    // More than eight thousand complete Int32 iterations precede overflow.
    // The test also requires the actual optimizing OSR overflow event below.
    function onceOverflow(limit) {
      let index = 0;
      let total = 2147475455;
      while (index < limit) {
        total = total + 1;
        index = index + 1;
      }
      return total;
    }
"#;

const POST_OSR_DEOPT_PROBE: &str = "String(onceOverflow(16384));";

const HOISTED_INVARIANT_READ_SETUP: &str = r#"
    function OsrHolder(value) { this.slot = value; }
    const osrHolder = new OsrHolder(7);

    function onceInvariantRead(holder, limit) {
      let total = 0;
      for (let index = 0; index < limit; index = index + 1) {
        total = total + holder.slot;
      }
      return total;
    }
"#;

const HOISTED_INVARIANT_READ_PROBE: &str = "String(onceInvariantRead(osrHolder, 4096));";

struct Observation {
    completion: String,
    before: RuntimeExecutionStats,
    after: RuntimeExecutionStats,
    artifacts: JitArtifactBatch,
    events: Vec<JitDebugEvent>,
    module: String,
}

fn run(setup: &str, probe: &str, selection: JitSelection, url: &str) -> Observation {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .expect("runtime");
    let module = format!("{url}-setup.js");
    runtime
        .run_script(SourceInput::from_javascript(setup), &module)
        .expect("OSR setup");
    let before = runtime.execution_stats();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(probe),
            &format!("{url}-probe.js"),
        )
        .expect("OSR fixture");
    let report = result.jit_debug_report().expect("OSR event capture");
    assert!(!report.truncated(), "native evidence must be complete");
    Observation {
        completion: result.completion_string().to_owned(),
        before,
        after: runtime.execution_stats(),
        artifacts: result
            .jit_artifacts()
            .expect("OSR artifact capture")
            .clone(),
        events: report.events().to_vec(),
        module,
    }
}

fn json(bundle: &JitArtifactBundle, file: JitArtifactFileName) -> serde_json::Value {
    serde_json::from_slice(bundle.file(file).expect("OSR artifact payload").contents()).unwrap()
}

fn assert_own_osr<'a>(observation: &'a Observation, function: &str) -> &'a JitArtifactBundle {
    assert!(
        observation.after.jit_optimized_osr_entries > observation.before.jit_optimized_osr_entries,
        "only {function} loops in the measured probe: before={:?}, after={:?}",
        observation.before,
        observation.after,
    );
    let bundle = observation
        .artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == observation.module
                && manifest.function_name() == function
                && manifest.tier() == JitDebugTier::Optimizing
                && matches!(manifest.entry(), JitDebugTarget::Osr { .. })
        })
        .unwrap_or_else(|| {
            panic!(
                "missing exact {function} OSR bundle: {:?}",
                observation.artifacts
            )
        });
    let manifest = bundle.manifest();
    assert_eq!(manifest.architecture(), std::env::consts::ARCH);
    assert_eq!(manifest.operating_system(), std::env::consts::OS);
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    assert_eq!(code.len() as u64, manifest.code_bytes());
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let JitDebugTarget::Osr { pc: loop_header_pc } = manifest.entry() else {
        unreachable!()
    };
    let entry = map["osrEntries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["logicalPc"].as_u64() == Some(u64::from(loop_header_pc)))
        .expect("the requested OSR header owns a native entry");
    let start = entry["startOffset"].as_u64().unwrap();
    let end = entry["endOffset"].as_u64().unwrap();
    assert!(start < end && end <= manifest.code_bytes());
    assert!(
        code[start as usize..end as usize]
            .iter()
            .any(|&byte| byte != 0)
    );
    let ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert!(ir.starts_with("; otter graph\n"));
    let regions = map["regions"].as_array().unwrap();
    let instructions = regions
        .iter()
        .filter(|region| region["kind"] == "instruction")
        .collect::<Vec<_>>();
    assert!(!instructions.is_empty());
    for region in instructions {
        assert_eq!(
            region["functionId"].as_u64(),
            Some(u64::from(manifest.function_id()))
        );
        let node = region["operationIndex"].as_u64().unwrap();
        assert!(
            ir.contains(&format!("  v{node} = ")),
            "own emitted Graph operation: {region}"
        );
        let start = region["startOffset"].as_u64().unwrap();
        let end = region["endOffset"].as_u64().unwrap();
        assert!(start <= end && end <= manifest.code_bytes());
    }
    bundle
}

fn assert_no_loop_exit(observation: &Observation, bundle: &JitArtifactBundle) {
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let latch = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["operation"]
                    .as_str()
                    .is_some_and(|operation| operation.contains(" JumpLoop("))
        })
        .map(|region| region["logicalPc"].as_u64().unwrap())
        .max()
        .expect("the subject owns an emitted native loop backedge");
    for event in &observation.events {
        let pc = match event {
            JitDebugEvent::Bail {
                function_id,
                tier: JitDebugTier::Optimizing,
                resume_pc,
                ..
            } if *function_id == bundle.manifest().function_id() => Some(*resume_pc),
            JitDebugEvent::EnteredGenerationDeopt {
                callee_function_id,
                callee_tier: JitDebugTier::Optimizing,
                callee_resume_pc,
                ..
            } if *callee_function_id == bundle.manifest().function_id() => Some(*callee_resume_pc),
            _ => None,
        };
        if let Some(pc) = pc {
            assert!(
                u64::from(pc) > latch,
                "native loop exited before completion: {event:?}"
            );
        }
    }
}

#[test]
fn once_called_float_array_loop_enters_optimized_osr() {
    let oracle = run(
        ARRAY_RMW_SETUP,
        ARRAY_RMW_PROBE,
        JitSelection::InterpreterOnly,
        "optimizing-osr-rmw-oracle.js",
    );
    let tiered = run(
        ARRAY_RMW_SETUP,
        ARRAY_RMW_PROBE,
        JitSelection::ProductionTiered,
        "optimizing-osr-rmw-tiered.js",
    );

    assert_eq!(tiered.completion, oracle.completion);
    assert_eq!(
        oracle.completion,
        r#"{"result":9216.5,"first":1.125,"last":1.125}"#
    );
    let bundle = assert_own_osr(&tiered, "onceFloatRmw");
    assert_no_loop_exit(&tiered, bundle);
}

#[test]
fn osr_entry_performs_the_pre_header_access_a_hoisted_loop_reads() {
    let oracle = run(
        HOISTED_INVARIANT_READ_SETUP,
        HOISTED_INVARIANT_READ_PROBE,
        JitSelection::InterpreterOnly,
        "optimizing-osr-hoist-oracle.js",
    );
    let tiered = run(
        HOISTED_INVARIANT_READ_SETUP,
        HOISTED_INVARIANT_READ_PROBE,
        JitSelection::ProductionTiered,
        "optimizing-osr-hoist-tiered.js",
    );

    assert_eq!(tiered.completion, oracle.completion);
    assert_eq!(oracle.completion, "28672");
    let bundle = assert_own_osr(&tiered, "onceInvariantRead");
    assert_no_loop_exit(&tiered, bundle);
}

#[test]
fn deopt_after_optimized_osr_reconstructs_loop_state() {
    let oracle = run(
        POST_OSR_DEOPT_SETUP,
        POST_OSR_DEOPT_PROBE,
        JitSelection::InterpreterOnly,
        "optimizing-osr-deopt-oracle.js",
    );
    let tiered = run(
        POST_OSR_DEOPT_SETUP,
        POST_OSR_DEOPT_PROBE,
        JitSelection::ProductionTiered,
        "optimizing-osr-deopt-tiered.js",
    );

    assert_eq!(tiered.completion, oracle.completion);
    assert_eq!(oracle.completion, "2147491839");
    let bundle = assert_own_osr(&tiered, "onceOverflow");
    assert!(
        tiered.after.jit_optimized_deopts > tiered.before.jit_optimized_deopts,
        "overflow after OSR must reconstruct and resume the frame: {:?}",
        tiered.after
    );
    let resume_pc = tiered
        .events
        .iter()
        .find_map(|event| match event {
            JitDebugEvent::Bail {
                function_id,
                tier: JitDebugTier::Optimizing,
                target: JitDebugTarget::Osr { .. },
                resume_pc,
                exit_reason: ExitReason::Int32Overflow,
                exit_action: ExitAction::Recompile,
                ..
            } if *function_id == bundle.manifest().function_id() => Some(*resume_pc),
            _ => None,
        })
        .expect("this exact subject must overflow in its optimizing OSR invocation");
    let deopt = json(bundle, JitArtifactFileName::Deopt);
    let exit = deopt["exits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|exit| {
            exit["reason"] == "int32Overflow"
                && exit["action"] == "recompile"
                && exit["resumePcs"].as_array().and_then(|pcs| pcs.last())
                    == Some(&serde_json::Value::from(resume_pc))
        })
        .expect("the observed overflow joins to this generation's deopt recipe");
    let state = deopt["frameStates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["id"] == exit["frameStateId"])
        .unwrap();
    let frames = state["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0]["functionId"].as_u64(),
        Some(u64::from(bundle.manifest().function_id()))
    );
    let map = json(bundle, JitArtifactFileName::CodeMap);
    assert!(
        map["regions"].as_array().unwrap().iter().any(|region| {
            region["kind"] == "instruction"
                && region["logicalPc"].as_u64() == Some(u64::from(resume_pc))
                && region["bytePc"] == frames[0]["bytePc"]
                && region["operation"]
                    .as_str()
                    .is_some_and(|operation| operation.contains("Int32Add"))
        }),
        "overflow recipe must resume its own emitted accumulator add"
    );
}
