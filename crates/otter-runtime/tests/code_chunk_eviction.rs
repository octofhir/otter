//! Dynamic bytecode chunk reclamation across runtime turns.
//!
//! # Contents
//!
//! - Dead, live, and mixed eval-chunk liveness cases.
//! - Exact payload-byte release and the separate aggregate source ledger.
//! - Source context optimization roots release before the actual dead-code census.
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

use otter_runtime::{JitSelection, ResourceAccount, ResourceClass, Runtime, SourceInput};

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
                "globalThis.__chunkKeeper = eval('(function kept() { return 42; })');",
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
    assert_eq!(
        stats_after_drop.evicted_chunks,
        stats_before_drop.evicted_chunks + 1
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
        JitSelection::ProductionTiered,
    ] {
        let account = ResourceAccount::default();
        let mut runtime = Runtime::builder()
            .resource_account(account.clone())
            .jit_selection(selection)
            .build()
            .expect("runtime");
        runtime.set_code_eviction_high_water_bytes(0);
        runtime
            .run_script(
                SourceInput::from_javascript(
                    "eval(`(function hot(n) {\
                       let sum = 0;\
                       for (let i = 0; i < n; i++) sum += i;\
                       return sum;\
                     })(2000)`);",
                ),
                "<hot-dead-eval>",
            )
            .expect("run hot eval");
        let generated_before = generated_code_bytes(&account);
        let source_before = source_module_bytes(&account);

        runtime.reclaim_dynamic_code().expect("reclaim hot eval");
        assert!(source_module_bytes(&account) < source_before);
        if selection == JitSelection::ProductionTiered {
            assert!(generated_before > 0, "hot eval must install generated code");
            assert!(generated_code_bytes(&account) < generated_before);
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
    assert_eq!(completions, ["42", "42"]);
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
        runtime.reclaim_dynamic_code().unwrap();
        assert_eq!(runtime.code_eviction_stats().evicted_chunks, 1);
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
