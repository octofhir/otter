//! Dynamic bytecode chunk reclamation across runtime turns.
//!
//! # Contents
//!
//! - Dead, live, and mixed eval-chunk liveness cases.
//! - Exact payload-byte release and the separate aggregate source ledger.
//! - Speculative source owners release before the actual dead-code census.
//! - Active compiler leases postpone eviction until their terminal acknowledgement.
//! - Bounded repeated-eval pressure and interpreter/JIT agreement.
//!
//! # Invariants
//!
//! - Reclamation runs only between top-level turns.
//! - A reachable function id or retained execution context keeps its payload.
//! - Reclaimed payload bytes are exact; aggregate `SourceModuleBytes` also
//!   includes immutable source text/index leases released with their last owner.
//!
//! # See also
//!
//! - `otter_vm::code_space`
//! - `otter_vm::code_liveness`

use otter_runtime::{
    JitDebugRequest, JitSelection, ResourceAccount, ResourceClass, Runtime, SourceInput,
};
use otter_vm::native_abi::{CodeLifetimeState, NativeFrameKind};

fn source_module_bytes(account: &ResourceAccount) -> u64 {
    account
        .snapshot()
        .get(ResourceClass::SourceModuleBytes)
        .current()
}

fn generated_code_bytes(account: &ResourceAccount) -> u64 {
    account
        .snapshot()
        .get(ResourceClass::GeneratedCodeBytes)
        .current()
}

fn reclaim_after_compiler_ack(runtime: &mut Runtime) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        runtime
            .reclaim_dynamic_code()
            .expect("reclaim dynamic source");
        let residency = runtime.jit_code_residency();
        // Once all reclaimable payload is gone, no pending job can block this
        // reclamation result. The zero-pressure fast return need not consume
        // further ACKs; waiting for queue-wide quiescence would not progress.
        if residency.pending_compilations == 0 || runtime.code_eviction_stats().retained_bytes == 0
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "compiler cancellation did not acknowledge: {residency:?}; {:?}",
            runtime.code_eviction_stats()
        );
        // A running compiler owns its immutable source until terminal ACK.
        // Retry that boundary without adding source or retraining a function.
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn dead_eval_chunk_releases_its_exact_source_ledger_charge() {
    let account = ResourceAccount::default();
    let mut runtime = Runtime::builder()
        .resource_account(account.clone())
        .build()
        .expect("runtime");
    runtime.set_code_eviction_high_water_bytes(0);

    runtime
        .run_script(
            SourceInput::from_javascript("eval('(function dead() { return 7; })'); undefined;"),
            "<dead-eval>",
        )
        .expect("eval script");
    let charged_before = source_module_bytes(&account);
    let payload_before = runtime.code_eviction_stats().retained_bytes;

    runtime.reclaim_dynamic_code().expect("reclaim dead eval");

    let stats = runtime.code_eviction_stats();
    let charged_after = source_module_bytes(&account);
    assert_eq!(stats.evicted_chunks, 1);
    assert!(stats.evicted_bytes > 0);
    assert!(stats.peak_retained_bytes >= stats.evicted_bytes);
    assert_eq!(stats.evicted_bytes, payload_before);
    assert!(
        charged_before - charged_after >= stats.evicted_bytes,
        "aggregate source release also includes immutable text/index leases"
    );
    assert_eq!(stats.retained_bytes, 0);
}

#[test]
fn escaped_eval_closure_blocks_eviction_until_the_reference_is_removed() {
    let account = ResourceAccount::default();
    let mut runtime = Runtime::builder()
        .resource_account(account.clone())
        .build()
        .expect("runtime");
    runtime.set_code_eviction_high_water_bytes(0);

    runtime
        .run_script(
            SourceInput::from_javascript(
                "globalThis.__chunkKeeper = eval('(() => { function kept() { return 42; } return kept; })()');",
            ),
            "<live-eval>",
        )
        .expect("store escaped closure");
    let charged_live = source_module_bytes(&account);

    runtime
        .reclaim_dynamic_code()
        .expect("census reachable closure");
    let live_stats = runtime.code_eviction_stats();
    assert_eq!(live_stats.evicted_chunks, 0);
    assert_eq!(live_stats.live_id_skips, 1);
    assert!(live_stats.retained_bytes > 0);
    assert_eq!(source_module_bytes(&account), charged_live);

    let result = runtime
        .run_script(
            SourceInput::from_javascript("__chunkKeeper();"),
            "<call-live-eval>",
        )
        .expect("escaped closure remains callable");
    assert_eq!(result.completion_string(), "42");
    // The call compiled `kept`'s body as a chunk of its own; the eval's
    // chunk, still reachable, keeps it.
    runtime
        .reclaim_dynamic_code()
        .expect("census the compiled body");
    assert_eq!(runtime.code_eviction_stats().evicted_chunks, 0);
    let again = runtime
        .run_script(
            SourceInput::from_javascript("__chunkKeeper();"),
            "<call-live-eval-again>",
        )
        .expect("compiled body survives with its eval");
    assert_eq!(again.completion_string(), "42");

    runtime
        .run_script(
            SourceInput::from_javascript("globalThis.__chunkKeeper = undefined;"),
            "<drop-live-eval>",
        )
        .expect("drop escaped closure");
    let charged_before_drop = source_module_bytes(&account);
    let stats_before_drop = runtime.code_eviction_stats();
    runtime
        .reclaim_dynamic_code()
        .expect("reclaim formerly live eval");
    let stats_after_drop = runtime.code_eviction_stats();
    // The eval's chunk and the body chunk of `kept` go together.
    assert_eq!(
        stats_after_drop.evicted_chunks,
        stats_before_drop.evicted_chunks + 2
    );
    let released_payload = stats_after_drop.evicted_bytes - stats_before_drop.evicted_bytes;
    assert_eq!(released_payload, stats_before_drop.retained_bytes);
    assert_eq!(stats_after_drop.retained_bytes, 0);
    assert!(
        charged_before_drop - source_module_bytes(&account) >= released_payload,
        "aggregate source release also includes immutable text/index leases"
    );
}

#[test]
fn mixed_live_and_dead_eval_chunks_reclaim_only_the_dead_payload() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime.set_code_eviction_high_water_bytes(0);
    runtime
        .run_script(
            SourceInput::from_javascript(
                "globalThis.__mixedKeeper = eval('(function kept() { return 19; })');\
                 eval('(function dead() { return 23; })');",
            ),
            "<mixed-eval>",
        )
        .expect("create mixed chunks");

    runtime.reclaim_dynamic_code().expect("reclaim mixed set");
    let stats = runtime.code_eviction_stats();
    assert_eq!(stats.evicted_chunks, 1);
    assert_eq!(stats.live_id_skips, 1);
    assert!(stats.retained_bytes > 0);
    let result = runtime
        .run_script(
            SourceInput::from_javascript("__mixedKeeper();"),
            "<call-mixed-keeper>",
        )
        .expect("live mixed closure remains callable");
    assert_eq!(result.completion_string(), "19");
}

#[test]
fn repeated_eval_pressure_is_reclaimed_automatically_on_the_next_turn() {
    const CHUNK_COUNT: u64 = 32;
    let account = ResourceAccount::default();
    let mut runtime = Runtime::builder()
        .resource_account(account.clone())
        .build()
        .expect("runtime");
    runtime.set_code_eviction_high_water_bytes(0);
    let mut source = String::new();
    for index in 0..CHUNK_COUNT {
        source.push_str(&format!(
            "eval('(function dead_{index}() {{ return {index}; }})');\n"
        ));
    }
    runtime
        .run_script(SourceInput::from_javascript(source), "<eval-pressure>")
        .expect("create bounded eval pressure");
    let charged_before = source_module_bytes(&account);

    runtime
        .run_script(SourceInput::from_javascript("0;"), "<reclaim-turn>")
        .expect("automatic reclaim turn");

    let stats = runtime.code_eviction_stats();
    assert_eq!(stats.evicted_chunks, CHUNK_COUNT);
    assert_eq!(stats.retained_bytes, 0);
    assert!(source_module_bytes(&account) < charged_before);
}

#[test]
fn interpreter_and_jit_agree_after_hot_eval_code_is_evicted() {
    let mut completions = Vec::new();
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let account = ResourceAccount::default();
        let mut runtime = Runtime::builder()
            .resource_account(account.clone())
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts())
            .build()
            .expect("runtime");
        let mut hot_codes = std::collections::BTreeSet::new();
        let mut hot_generation = None;
        // The source stays semantically live while ordinary calls earn an exact
        // current native entry. A fast loop alone can finish before its worker
        // publishes code, especially in checked/release builds.
        for batch in 0..=256 {
            let pending = runtime.jit_code_residency().pending_compilations != 0;
            let source = if batch == 0 {
                "globalThis.__hotChunk = eval(`(function hot(n) {\
                       let sum = 0;\
                       for (let i = 0; i < n; i++) sum += i;\
                       return sum;\
                     })`); __hotChunk(2000);"
            } else if pending {
                "0;"
            } else {
                "__hotChunk(2000);"
            };
            let result = runtime
                .run_script(SourceInput::from_javascript(source), "<hot-eval-admission>")
                .expect("run or acknowledge hot eval");
            assert_eq!(
                result.completion_string(),
                if batch != 0 && pending {
                    "0"
                } else {
                    "1999000"
                }
            );
            let artifacts = result.jit_artifacts().unwrap();
            assert!(!artifacts.truncated());
            for bundle in artifacts.bundles() {
                if bundle.manifest().function_name() == "hot" {
                    hot_codes.insert(bundle.manifest().code_object_id());
                }
            }
            if selection == JitSelection::InterpreterOnly {
                break;
            }
            let tier = if selection == JitSelection::Template {
                NativeFrameKind::Baseline
            } else {
                NativeFrameKind::Optimizing
            };
            hot_generation = runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .find(|code| {
                    hot_codes.contains(&code.code_object_id)
                        && code.lifecycle == CodeLifetimeState::Installed
                        && code.linked
                        && code.current_entry
                        && code.call_entry_offset.is_some()
                        && code.tier == tier
                });
            if hot_generation.is_some() && runtime.jit_code_residency().pending_compilations == 0 {
                break;
            }
            assert!(
                batch < 256,
                "{selection:?}: exact hot native admission; {:?}",
                runtime.jit_code_generation_snapshot()
            );
            std::thread::yield_now();
        }
        runtime
            .run_script(
                SourceInput::from_javascript("globalThis.__hotChunk = undefined;"),
                "<drop-hot-eval>",
            )
            .expect("drop the semantic source owner");
        assert_eq!(runtime.jit_code_residency().pending_compilations, 0);
        let generated_before = generated_code_bytes(&account);
        let source_before = source_module_bytes(&account);
        let payload_before = runtime.code_eviction_stats().retained_bytes;
        let evicted_before = runtime.code_eviction_stats().evicted_bytes;

        runtime.set_code_eviction_high_water_bytes(0);
        reclaim_after_compiler_ack(&mut runtime);
        assert_eq!(runtime.code_eviction_stats().retained_bytes, 0);
        assert_eq!(
            runtime.code_eviction_stats().evicted_bytes - evicted_before,
            payload_before
        );
        assert!(
            source_module_bytes(&account) < source_before,
            "{selection:?}: {:?}; residency: {:?}",
            runtime.code_eviction_stats(),
            runtime.jit_code_residency()
        );
        if let Some(hot) = hot_generation {
            assert!(generated_before > 0, "hot eval must install generated code");
            assert!(generated_code_bytes(&account) < generated_before);
            let retired = runtime
                .jit_code_generation_snapshot()
                .into_iter()
                .find(|code| code.code_object_id == hot.code_object_id)
                .unwrap();
            assert_eq!(retired.lifecycle, CodeLifetimeState::Retired);
            assert_eq!(retired.active_count, 0);
        }
        let result = runtime
            .run_script(
                SourceInput::from_javascript(
                    "eval('(function add(left, right) { return left + right; })')(20, 22);",
                ),
                "<post-eviction-eval>",
            )
            .expect("execute after eviction");
        completions.push(result.completion_string().to_string());
    }
    assert_eq!(completions, ["42", "42", "42"]);
}

#[test]
fn captured_eval_contexts_do_not_keep_dead_source_chunks_alive() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
            globalThis.__contextKeeper = eval(`(function captureSource() {
                const seed = 7;
                const read = function readEvalContext() { return seed + arguments.length; };
                let total = 0;
                for (let call = 0; call < 12000; call++) total += read();
                if (total !== 84000) throw new Error('context result');
                return read;
            })()`);
            String(__contextKeeper());
        "#,
                ),
                "<captured-eval>",
            )
            .unwrap();
        assert_eq!(result.completion_string(), "7");
        runtime.set_code_eviction_high_water_bytes(0);
        runtime.reclaim_dynamic_code().unwrap();
        assert_eq!(runtime.code_eviction_stats().evicted_chunks, 0);
        assert!(runtime.code_eviction_stats().retained_bytes > 0);
        let result = runtime
            .run_script(
                SourceInput::from_javascript("String(__contextKeeper());"),
                "<live-context-call>",
            )
            .unwrap();
        assert_eq!(result.completion_string(), "7");
        runtime
            .run_script(
                SourceInput::from_javascript("globalThis.__contextKeeper = undefined;"),
                "<drop-context>",
            )
            .unwrap();
        reclaim_after_compiler_ack(&mut runtime);
        // The eval's chunk and the body chunk `readEvalContext` compiled on
        // its first call.
        assert_eq!(
            runtime.code_eviction_stats().evicted_chunks,
            2,
            "{selection:?}: {:?}; residency: {:?}",
            runtime.code_eviction_stats(),
            runtime.jit_code_residency()
        );
        assert_eq!(runtime.code_eviction_stats().retained_bytes, 0);
    }
}

#[test]
fn live_code_past_the_mark_is_not_censused_on_every_turn() {
    const TURNS: u64 = 20;
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(
                "globalThis.__kept = eval('(function kept() { return 42; })');",
            ),
            "<live-eval>",
        )
        .expect("store a live eval closure");
    runtime.set_code_eviction_high_water_bytes(1);
    for turn in 0..TURNS {
        let result = runtime
            .run_script(SourceInput::from_javascript("__kept();"), "<turn>")
            .expect("a turn");
        assert_eq!(result.completion_string(), "42", "turn {turn}");
    }
    let stats = runtime.code_eviction_stats();
    assert_eq!(stats.evicted_chunks, 0, "the chunk is live");
    assert!(
        stats.census_passes < 3,
        "a census that freed nothing is not repeated every turn: {} passes in {TURNS} turns",
        stats.census_passes
    );

    // Growth past the raised mark is looked at again.
    let mut source = String::new();
    for index in 0..64 {
        source.push_str(&format!(
            "eval('(function dead_{index}() {{ return {index}; }})');\n"
        ));
    }
    runtime
        .run_script(SourceInput::from_javascript(source), "<growth>")
        .expect("create dead eval chunks");
    runtime
        .run_script(SourceInput::from_javascript("0;"), "<reclaim-turn>")
        .expect("a turn after growth");
    assert_eq!(runtime.code_eviction_stats().evicted_chunks, 64);
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[path = "code_chunk_eviction/native_retention.rs"]
mod native_retention;

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[path = "code_chunk_eviction/compiled_constants.rs"]
mod compiled_constants;
