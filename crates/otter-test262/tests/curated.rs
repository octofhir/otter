//! Curated bring-up subset for the per-test driver (slice 103).
//!
//! Synthetic fixtures that exercise every [`Outcome`] variant on
//! the in-process driver. CI uses these to detect regressions in
//! the runner itself before it touches the full corpus.
//!
//! Spec: <https://github.com/tc39/test262/blob/main/INTERPRETING.md>

use std::path::Path;
use std::time::Duration;

use otter_test262::config::Test262Config;
use otter_test262::harness::HarnessCache;
use otter_test262::runner::{CorpusPaths, ExecConfig, Outcome, run_one};

/// Build a synthetic [`CorpusPaths`] rooted at `tmp` with a
/// minimal `harness/` containing `assert.js` + `sta.js`.
fn synth_corpus(tmp: &tempfile::TempDir) -> CorpusPaths {
    let root = tmp.path().to_path_buf();
    let test_dir = root.join("test");
    let harness_dir = root.join("harness");
    std::fs::create_dir_all(&test_dir).unwrap();
    std::fs::create_dir_all(&harness_dir).unwrap();

    // Minimal `assert` API — enough for the curated tests below.
    // The real harness is loaded straight from `vendor/test262`
    // when the CLI runs the corpus; here we synthesise just what
    // these unit tests touch.
    // Minimal harness as a plain object so we exercise the engine's
    // `throw new Error(...)` path without depending on
    // function-as-object property assignment.
    std::fs::write(
        harness_dir.join("assert.js"),
        r#"
var assert = {
    sameValue: function (actual, expected) {
        if (actual !== expected) {
            throw new Error("sameValue failed");
        }
    },
    notSameValue: function (actual, unexpected) {
        if (actual === unexpected) {
            throw new Error("notSameValue failed");
        }
    },
    throws: function (ctor, fn) {
        var thrown = false;
        try { fn(); } catch (e) { thrown = true; }
        if (!thrown) {
            throw new Error("expected throw");
        }
    }
};
"#,
    )
    .unwrap();
    std::fs::write(harness_dir.join("sta.js"), "// sta.js stub\n").unwrap();

    CorpusPaths {
        root,
        test_dir,
        harness_dir,
    }
}

fn write_test(corpus: &CorpusPaths, rel: &str, source: &str) -> std::path::PathBuf {
    let path = corpus.test_dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, source).unwrap();
    path
}

fn driver_config() -> ExecConfig {
    ExecConfig {
        jit_selection: otter_runtime::JitSelection::Baseline,
        timeout: Duration::from_millis(5_000),
        max_heap_bytes: 256 * 1024 * 1024,
        config: Test262Config::default(),
    }
}

fn drive(corpus: &CorpusPaths, path: &Path) -> Outcome {
    let mut harness = HarnessCache::new(&corpus.harness_dir);
    let cfg = driver_config();
    run_one(path, corpus, &mut harness, &cfg).outcome
}

#[test]
fn pass_outcome_when_test_returns_normally() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "pass/basic.js",
        "/*---\ndescription: trivial pass\n---*/\nassert.sameValue(1 + 1, 2);\n",
    );
    let outcome = drive(&corpus, &path);
    assert!(matches!(outcome, Outcome::Pass), "got {outcome:?}");
}

#[test]
fn create_realm_uses_distinct_global_and_eval_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "realm/basic.js",
        r#"/*---
description: createRealm exposes a separate global and evalScript scope
---*/
const realm = $262.createRealm();
assert.sameValue(realm.global === globalThis, false);
assert.sameValue(realm.global.TypeError === TypeError, false);
realm.evalScript("var realmOnly = 42;");
assert.sameValue(realm.global.realmOnly, 42);
assert.sameValue(typeof globalThis.realmOnly, "undefined");
"#,
    );
    let outcome = drive(&corpus, &path);
    assert!(matches!(outcome, Outcome::Pass), "got {outcome:?}");
}

#[test]
fn escaped_bytecode_function_raises_in_its_origin_realm() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "realm/escaped-function-error.js",
        r#"/*---
description: escaped bytecode functions retain their origin error realm
flags: [onlyStrict]
features: [cross-realm, Proxy]
---*/
const other = $262.createRealm();
const callRevoked = other.evalScript(`
  (function() {
    var proxyObj = Proxy.revocable(function() {}, {});
    var proxy = proxyObj.proxy;
    var revoke = proxyObj.revoke;
    revoke();
    return proxy();
  })
`);
let caught = null;
try {
  callRevoked();
} catch (error) {
  caught = error;
}
assert.sameValue(caught instanceof other.global.TypeError, true);
assert.sameValue(caught instanceof TypeError, false);
"#,
    );
    let outcome = drive(&corpus, &path);
    assert!(matches!(outcome, Outcome::Pass), "got {outcome:?}");
}

#[test]
fn create_realm_preserves_iterator_realm_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "realm/iterator.js",
        r#"/*---
description: iterator helpers use the realm of the called method
---*/
const other = $262.createRealm().global;
const iter = [1, 2, 3].values();
assert.sameValue(Iterator.from(iter), iter);
assert.sameValue(other.Iterator.from(iter) === iter, false);

const arr = other.Iterator.prototype.toArray.call([1, 2, 3].values());
assert.sameValue(arr instanceof Array, false);
assert.sameValue(arr instanceof other.Array, true);

let caught;
try {
    other.Iterator.prototype.every.call([].values());
} catch (e) {
    caught = e;
}
assert.sameValue(caught instanceof other.TypeError, true);
assert.sameValue(caught instanceof TypeError, false);
"#,
    );
    let outcome = drive(&corpus, &path);
    assert!(matches!(outcome, Outcome::Pass), "got {outcome:?}");
}

#[test]
fn default_script_runs_strict_variant_too() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "strictness/default-sloppy.js",
        "/*---\ndescription: default script runs both sloppy and strict variants\n---*/\nassert.sameValue((function() { return this; })(), globalThis);\n",
    );
    let outcome = drive(&corpus, &path);
    match outcome {
        Outcome::Fail { reason, .. } => assert!(reason.contains("strict"), "reason was: {reason}"),
        other => panic!("expected strict variant failure, got {other:?}"),
    }
}

#[test]
fn only_strict_flag_gets_strict_prelude() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "strictness/only-strict.js",
        "/*---\ndescription: onlyStrict runs under strict source\nflags: [onlyStrict]\n---*/\nassert.sameValue((function() { return this; })(), undefined);\n",
    );
    let outcome = drive(&corpus, &path);
    assert!(matches!(outcome, Outcome::Pass), "got {outcome:?}");
}

#[test]
fn no_strict_flag_runs_without_strict_prelude() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "strictness/no-strict.js",
        "/*---\ndescription: noStrict runs as sloppy script\nflags: [noStrict]\n---*/\nassert.sameValue((function() { return this; })(), globalThis);\n",
    );
    let outcome = drive(&corpus, &path);
    assert!(matches!(outcome, Outcome::Pass), "got {outcome:?}");
}

#[test]
fn fail_outcome_when_assert_throws() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "fail/sameValue.js",
        "/*---\ndescription: should fail\n---*/\nassert.sameValue(1, 2);\n",
    );
    let outcome = drive(&corpus, &path);
    match outcome {
        Outcome::Fail { reason, .. } => {
            assert!(reason.contains("runtime"), "reason was: {reason}");
        }
        other => panic!("expected Fail, got {other:?}"),
    }
}

#[test]
fn negative_runtime_pass_when_typeerror_thrown() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "negative/typeerror-throws.js",
        "/*---\ndescription: throws TypeError on null member access\nnegative:\n  phase: runtime\n  type: TypeError\n---*/\n(null).foo;\n",
    );
    let outcome = drive(&corpus, &path);
    assert!(matches!(outcome, Outcome::Pass), "got {outcome:?}");
}

#[test]
fn negative_parse_pass_when_syntaxerror() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    // `for (let x = 1, x = 2;;) {}` — duplicate `let` names in the
    // same lexical declaration. Spec §16.1.5 SS: Early Errors.
    // This is a syntactic failure caught by the foundation parser.
    let path = write_test(
        &corpus,
        "negative/early-syntax.js",
        "/*---\ndescription: duplicate let binding\nnegative:\n  phase: parse\n  type: SyntaxError\n---*/\nfor (let x = 1, x = 2;;) {}\n",
    );
    let outcome = drive(&corpus, &path);
    assert!(matches!(outcome, Outcome::Pass), "got {outcome:?}");
}

#[test]
fn skipped_outcome_for_skip_feature() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "skip/atomics.js",
        "/*---\ndescription: skip via feature\nfeatures: [Atomics]\n---*/\n1;\n",
    );
    let mut harness = HarnessCache::new(&corpus.harness_dir);
    let cfg = ExecConfig {
        jit_selection: otter_runtime::JitSelection::Baseline,
        timeout: Duration::from_millis(5_000),
        max_heap_bytes: 256 * 1024 * 1024,
        config: {
            let mut c = Test262Config::default();
            c.skip_features.push("Atomics".to_string());
            c
        },
    };
    let result = run_one(&path, &corpus, &mut harness, &cfg);
    match result.outcome {
        Outcome::Skipped { feature } => assert_eq!(feature, "Atomics"),
        other => panic!("expected Skipped(Atomics), got {other:?}"),
    }
}

#[test]
fn skipped_outcome_for_no_strict_only_test() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "skip/no-strict.js",
        "/*---\ndescription: noStrict only\nflags: [noStrict]\n---*/\n1;\n",
    );
    let mut harness = HarnessCache::new(&corpus.harness_dir);
    let cfg = ExecConfig {
        jit_selection: otter_runtime::JitSelection::Baseline,
        timeout: Duration::from_millis(5_000),
        max_heap_bytes: 256 * 1024 * 1024,
        config: {
            let mut c = Test262Config::default();
            c.skip_flags.push("noStrict".to_string());
            c
        },
    };
    let result = run_one(&path, &corpus, &mut harness, &cfg);
    match result.outcome {
        Outcome::Skipped { feature } => assert_eq!(feature, "flag:noStrict"),
        other => panic!("expected flag:noStrict skip, got {other:?}"),
    }
}

#[test]
fn skipped_outcome_for_known_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "panics/foo.js",
        "/*---\ndescription: known panic\n---*/\n1;\n",
    );
    let mut harness = HarnessCache::new(&corpus.harness_dir);
    let cfg = ExecConfig {
        jit_selection: otter_runtime::JitSelection::Baseline,
        timeout: Duration::from_millis(5_000),
        max_heap_bytes: 256 * 1024 * 1024,
        config: {
            let mut c = Test262Config::default();
            c.known_panics.push("panics/foo".to_string());
            c
        },
    };
    let result = run_one(&path, &corpus, &mut harness, &cfg);
    match result.outcome {
        Outcome::Skipped { feature } => assert_eq!(feature, "known panic"),
        other => panic!("expected known panic skip, got {other:?}"),
    }
}

#[test]
fn skipped_outcome_for_ignored_path() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "skip/ignored-here.js",
        "/*---\ndescription: ignored by config\n---*/\n1;\n",
    );
    let mut harness = HarnessCache::new(&corpus.harness_dir);
    let cfg = ExecConfig {
        jit_selection: otter_runtime::JitSelection::Baseline,
        timeout: Duration::from_millis(5_000),
        max_heap_bytes: 256 * 1024 * 1024,
        config: {
            let mut c = Test262Config::default();
            c.ignored_tests.push("skip/ignored-here".to_string());
            c
        },
    };
    let result = run_one(&path, &corpus, &mut harness, &cfg);
    match result.outcome {
        Outcome::Skipped { feature } => assert_eq!(feature, "ignored by config"),
        other => panic!("expected ignored skip, got {other:?}"),
    }
}

#[test]
fn timeout_outcome_when_busy_loop_exceeds_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "timeout/busy-loop.js",
        "/*---\ndescription: deliberate infinite loop\n---*/\nwhile (true) {}\n",
    );
    let mut harness = HarnessCache::new(&corpus.harness_dir);
    // Short, deliberate budget so the test finishes quickly.
    let cfg = ExecConfig {
        jit_selection: otter_runtime::JitSelection::Baseline,
        timeout: Duration::from_millis(500),
        max_heap_bytes: 256 * 1024 * 1024,
        config: Test262Config::default(),
    };
    let result = run_one(&path, &corpus, &mut harness, &cfg);
    match result.outcome {
        Outcome::Timeout { .. } => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[test]
fn negative_pass_outcome_fails_when_no_throw() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(
        &corpus,
        "negative/should-have-thrown.js",
        "/*---\ndescription: expected throw\nnegative:\n  phase: runtime\n  type: TypeError\n---*/\n1;\n",
    );
    match drive(&corpus, &path) {
        Outcome::Fail { reason, .. } => assert!(reason.contains("expected TypeError")),
        other => panic!("expected Fail, got {other:?}"),
    }
}

#[test]
fn missing_frontmatter_records_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let corpus = synth_corpus(&tmp);
    let path = write_test(&corpus, "no-fm/foo.js", "1 + 1;\n");
    match drive(&corpus, &path) {
        Outcome::Skipped { feature } => assert_eq!(feature, "no frontmatter"),
        other => panic!("expected Skipped, got {other:?}"),
    }
}
