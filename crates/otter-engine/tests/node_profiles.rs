use otter_engine::{EngineBuilder, NodeApiProfile};

fn assert_ok(otter: &mut otter_engine::Otter, code: &str) {
    let value = otter
        .eval_sync(code)
        .unwrap_or_else(|e| panic!("Eval failed: {e}"));
    let out = value.as_string().map(|s| s.to_string()).unwrap_or_default();
    assert_eq!(out, "ok");
}

fn assert_err_contains(otter: &mut otter_engine::Otter, code: &str, needle: &str) {
    let err = otter
        .eval_sync(code)
        .expect_err("Expected eval to fail")
        .to_string();
    assert!(
        err.contains(needle),
        "Expected '{needle}' in error, got '{err}'"
    );
}

fn assert_err_contains_any(otter: &mut otter_engine::Otter, code: &str, needles: &[&str]) {
    let err = otter
        .eval_sync(code)
        .expect_err("Expected eval to fail")
        .to_string();
    assert!(
        needles.iter().any(|needle| err.contains(needle)),
        "Expected one of {:?} in error, got '{err}'",
        needles
    );
}

#[test]
fn test_none_profile_blocks_fs_promises_prefixed_and_bare() {
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::None)
        .build();
    assert_err_contains_any(
        &mut otter,
        "import fsp from 'node:fs/promises'; fsp.readFile;",
        &["node:fs/promises", "Cannot read property of non-object"],
    );
    assert_err_contains_any(
        &mut otter,
        "import fsp from 'fs/promises'; fsp.readFile;",
        &["fs/promises", "Cannot read property of non-object"],
    );
}

#[test]
fn test_safe_core_allows_assert_util_events_and_assert_strict() {
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::SafeCore)
        .build();
    assert_ok(
        &mut otter,
        "import assertStrict from 'node:assert/strict'; import util from 'node:util'; import { EventEmitter } from 'node:events'; assertStrict.equal(1, 1); const ee = new EventEmitter(); ee.on('x', () => {}); if (!util.types.isArray([])) throw new Error('bad'); 'ok';",
    );
}

#[test]
fn test_safe_core_allows_timers_modules() {
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::SafeCore)
        .build();
    assert_ok(
        &mut otter,
        "import timers from 'node:timers'; import * as tp from 'node:timers/promises';\n\
         if (typeof timers.setTimeout !== 'function') throw new Error('timers.setTimeout');\n\
         if (typeof timers.setImmediate !== 'function') throw new Error('timers.setImmediate');\n\
         if (typeof tp.setTimeout !== 'function') throw new Error('timers/promises.setTimeout');\n\
         if (typeof tp.setImmediate !== 'function') throw new Error('timers/promises.setImmediate');\n\
         if (typeof tp.setInterval !== 'function') throw new Error('timers/promises.setInterval');\n\
         if (typeof tp.scheduler?.wait !== 'function') throw new Error('timers/promises.scheduler.wait');\n\
         'ok';",
    );
}

#[test]
fn test_safe_core_blocks_process_and_fs() {
    let mut otter_process = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::SafeCore)
        .build();
    assert_err_contains(
        &mut otter_process,
        "import process from 'node:process'; process.pid;",
        "node:process",
    );

    let mut otter_fs = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::SafeCore)
        .build();
    assert_err_contains(
        &mut otter_fs,
        "import fs from 'node:fs'; fs.readFileSync('/tmp/x', 'utf8');",
        "node:fs",
    );
}

#[test]
fn test_full_profile_allows_process_and_fs_module_load() {
    let mut otter = EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .build();
    assert_ok(
        &mut otter,
        "import process from 'node:process'; import fs from 'node:fs'; if (typeof process.cwd !== 'function') throw new Error('bad process'); if (typeof fs.readFileSync !== 'function') throw new Error('bad fs'); 'ok';",
    );
}
