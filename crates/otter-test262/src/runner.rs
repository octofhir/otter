//! Corpus traversal + per-test execution driver for the Test262
//! runner.
//!
//! Slice 101 shipped the bare walk; slice 103 layers the per-test
//! [`Outcome`] taxonomy and [`run_one`] driver on top. The driver:
//!
//! 1. Skip via `test262_config.toml` (`known_panics`,
//!    `ignored_tests`).
//! 2. Parse frontmatter; on parse failure → [`Outcome::Fail`].
//! 3. Skip via `skip_features` (config-driven).
//! 4. Skip via `skip_flags` (config-driven escape hatch for
//!    unsupported host/test modes).
//! 5. Build a fresh `Runtime` with the configured heap cap.
//! 6. Compile + run the harness preamble (cached per-worker).
//! 7. Compile + run the test body. `flags: [module]` routes through
//!    [`otter_runtime::Runtime::run_module`]; dynamic-import scripts
//!    are staged on disk so sibling `_FIXTURE.js` imports resolve;
//!    other scripts route through [`otter_runtime::Runtime::run_script`].
//! 8. Map the engine outcome onto [`Outcome`] per ECMA-262 +
//!    test262 INTERPRETING.md negative-test rules.
//!
//! Spec: <https://tc39.es/ecma262/>
//! Spec: <https://github.com/tc39/test262/blob/main/INTERPRETING.md>

use std::path::{Path, PathBuf};
use std::time::Duration;

use ignore::WalkBuilder;
use otter_runtime::{Diagnostic, DiagnosticKind, IoErrorKind, OtterError, Runtime, SourceInput};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::Test262Config;
use crate::feature_map::FeatureMap;
use crate::harness::HarnessCache;
use crate::isolation::{WatchdogOutcome, fresh_runtime, run_with_watchdog};
use crate::metadata::{Frontmatter, FrontmatterError, NegativePhase, TestFlag};

/// Resolved on-disk paths for a test262 checkout.
#[derive(Debug, Clone)]
pub struct CorpusPaths {
    /// Root of the submodule (`vendor/test262`).
    pub root: PathBuf,
    /// Test tree (`vendor/test262/test`).
    pub test_dir: PathBuf,
    /// Harness fragments (`vendor/test262/harness`).
    pub harness_dir: PathBuf,
}

/// Locate the test262 corpus on disk.
///
/// # Errors
/// - [`CorpusError::Missing`] when `vendor/test262` does not exist.
/// - [`CorpusError::Empty`] when `vendor/test262/test` is missing
///   or empty (uninitialised submodule).
pub fn ensure_corpus_present(repo_root: &Path) -> Result<CorpusPaths, CorpusError> {
    let root = repo_root.join("vendor").join("test262");
    if !root.exists() {
        return Err(CorpusError::Missing { root });
    }
    let test_dir = root.join("test");
    let harness_dir = root.join("harness");
    if !test_dir.is_dir() {
        return Err(CorpusError::Empty { root });
    }
    let mut entries = std::fs::read_dir(&test_dir).map_err(|e| CorpusError::Io {
        path: test_dir.clone(),
        message: e.to_string(),
    })?;
    if entries.next().is_none() {
        return Err(CorpusError::Empty { root });
    }
    Ok(CorpusPaths {
        root,
        test_dir,
        harness_dir,
    })
}

/// Walk the test262 `test/` tree and return every test path.
///
/// `_FIXTURE.js` files are excluded per
/// [INTERPRETING.md](https://github.com/tc39/test262/blob/main/INTERPRETING.md#test-files).
/// `filter` (when supplied) is a substring match on the path
/// relative to `paths.test_dir`; a leading `^` anchors it to the
/// start of the path so batch runners can keep directory shards
/// disjoint (`built-ins/RegExp/` would otherwise also match
/// `annexB/built-ins/RegExp/...`).
pub fn list_tests(paths: &CorpusPaths, filter: Option<&str>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let walker = WalkBuilder::new(&paths.test_dir)
        .standard_filters(true)
        .git_ignore(true)
        .git_exclude(true)
        .hidden(false)
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension() else {
            continue;
        };
        if ext != "js" {
            continue;
        }
        let path_str = path.to_string_lossy();
        if path_str.ends_with("_FIXTURE.js") {
            continue;
        }
        if let Some(filter) = filter {
            let rel = path
                .strip_prefix(&paths.test_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let matched = match filter.strip_prefix('^') {
                Some(prefix) => rel.starts_with(prefix),
                None => rel.contains(filter),
            };
            if !matched {
                continue;
            }
        }
        out.push(path.to_path_buf());
    }
    out.sort();
    out
}

/// Same as [`list_tests`] but only returns the count.
#[must_use]
pub fn count_tests(paths: &CorpusPaths, filter: Option<&str>) -> usize {
    list_tests(paths, filter).len()
}

/// The per-test outcome taxonomy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// Test body returned normally (or threw the spec-required
    /// negative error).
    Pass,
    /// Test failed conformance.
    Fail {
        /// Human-readable reason (rendered into the report).
        reason: String,
        /// Optional engine stack trace.
        stack: Option<String>,
    },
    /// Test was skipped — config / strict-mode policy / unsupported
    /// feature.
    Skipped {
        /// What was skipped (`<feature>` / `"foundation-always-strict"` /
        /// `"ignored by config"` / `"known panic"`).
        feature: String,
    },
    /// Engine panicked while running the test.
    Crash {
        /// Panic payload (rendered).
        panic: String,
    },
    /// Per-test wall-clock budget exceeded.
    Timeout {
        /// Configured budget that fired (ms).
        ms: u64,
    },
    /// Engine heap cap fired.
    OutOfMemory {
        /// Bytes the engine reported as "requested at cap".
        bytes: u64,
    },
}

/// Per-test result record consumed by the report writer (slice 104).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// Test path relative to `vendor/test262/test/`.
    pub path: String,
    /// `esid:` from the frontmatter (if any).
    pub esid: Option<String>,
    /// Decoded `features:` for the test.
    pub features: Vec<String>,
    /// Final outcome.
    pub outcome: Outcome,
    /// Wall-clock duration.
    pub wall_ms: u64,
}

/// Configuration shared across all `run_one` calls in a sweep.
#[derive(Debug, Clone)]
pub struct ExecConfig {
    /// Per-test wall-clock budget.
    pub timeout: Duration,
    /// Per-test heap cap (bytes); `0` disables.
    pub max_heap_bytes: u64,
    /// Loaded `test262_config.toml` (drives skip lists).
    pub config: Test262Config,
}

impl ExecConfig {
    /// Build the in-memory feature map from the loaded config.
    #[must_use]
    pub fn feature_map(&self) -> FeatureMap {
        FeatureMap::from_skip_features(self.config.skip_features.clone())
    }
}

/// Run a single test through the foundation-phase driver. The
/// driver allocates a fresh `Runtime` per test, applies the
/// timeout + heap cap, and never panics — engine panics surface
/// as [`Outcome::Crash`] so a single bad test cannot derail the
/// sweep.
///
/// `paths` must be a fresh [`CorpusPaths`] (typically from
/// [`ensure_corpus_present`]). `harness` is shared across tests so
/// the `assert.js` / `sta.js` / `includes` reads are paid once per
/// worker.
pub fn run_one(
    test_path: &Path,
    paths: &CorpusPaths,
    harness: &mut HarnessCache,
    exec: &ExecConfig,
) -> TestResult {
    let start = std::time::Instant::now();
    let rel_path = relative_path(test_path, &paths.test_dir);

    // 1. Skip via config (no fs read needed). Cheap and lets us
    //    short-circuit known-bad files.
    if exec.config.is_known_panic(&rel_path) {
        return result_with(
            rel_path,
            None,
            Vec::new(),
            Outcome::Skipped {
                feature: "known panic".to_string(),
            },
            start,
        );
    }
    if exec.config.is_ignored(&rel_path) {
        return result_with(
            rel_path,
            None,
            Vec::new(),
            Outcome::Skipped {
                feature: "ignored by config".to_string(),
            },
            start,
        );
    }

    // 2. Read the source.
    let source = match std::fs::read_to_string(test_path) {
        Ok(text) => text,
        Err(err) => {
            return result_with(
                rel_path,
                None,
                Vec::new(),
                Outcome::Fail {
                    reason: format!("failed to read test file: {err}"),
                    stack: None,
                },
                start,
            );
        }
    };

    // 3. Refuse oversize inputs (defence in depth — the corpus
    //    has no >2 MB tests today).
    if source.len() > 2 * 1024 * 1024 {
        return result_with(
            rel_path,
            None,
            Vec::new(),
            Outcome::Skipped {
                feature: "source too large".to_string(),
            },
            start,
        );
    }

    // 4. Parse frontmatter.
    let frontmatter = match Frontmatter::parse(&source) {
        Ok(fm) => fm,
        Err(FrontmatterError::MissingBlock) => {
            return result_with(
                rel_path,
                None,
                Vec::new(),
                Outcome::Skipped {
                    feature: "no frontmatter".to_string(),
                },
                start,
            );
        }
        Err(err) => {
            return result_with(
                rel_path,
                None,
                Vec::new(),
                Outcome::Fail {
                    reason: format!("frontmatter parse failed: {err}"),
                    stack: None,
                },
                start,
            );
        }
    };

    let features = frontmatter.features.clone();
    let esid = frontmatter.esid.clone();

    // 5. Skip via feature_map.
    let feature_map = exec.feature_map();
    if let Some(feat) = feature_map.first_skipped(&features).map(str::to_string) {
        return result_with(
            rel_path,
            esid,
            features,
            Outcome::Skipped { feature: feat },
            start,
        );
    }

    // 6. Configured flag policy. This remains a generic escape
    //    hatch for unsupported host/test modes; strictness itself
    //    is honored from Test262 frontmatter below.
    if let Some(flag) = exec.config.first_skipped_flag(&frontmatter.flags) {
        return result_with(
            rel_path,
            esid,
            features,
            Outcome::Skipped {
                feature: format!("flag:{flag}"),
            },
            start,
        );
    }

    // 7. Build harness preamble.
    let preamble = match harness.preamble_for(&frontmatter) {
        Ok(text) => text,
        Err(err) => {
            return result_with(
                rel_path,
                esid,
                features,
                Outcome::Fail {
                    reason: format!("harness load failed: {err}"),
                    stack: None,
                },
                start,
            );
        }
    };

    // 8. Build the per-test source. Module tests keep the harness
    // separate: it must evaluate as a classic script in the global
    // scope (INTERPRETING.md) so `assert` / `Test262Error` are global
    // bindings visible to every module in the test's import graph —
    // sibling test files imported as dependencies reference them too.
    let body = Frontmatter::body_of(&source);
    let allow_blocking_atomics_wait = !frontmatter
        .test_flags()
        .contains(&TestFlag::CanBlockIsFalse);

    let mapped = if frontmatter.is_module() {
        let outcome = run_module_with_fresh_runtime(
            exec,
            allow_blocking_atomics_wait,
            &preamble,
            body,
            test_path,
        );
        invert_negative(outcome, frontmatter.negative.as_ref(), exec.timeout)
    } else if frontmatter.is_raw() {
        // `flags: [raw]` — INTERPRETING.md mandates the file run
        // verbatim: no harness preamble, no strict prologue, and no
        // frontmatter stripping. Stripping the frontmatter would also
        // discard a leading hashbang (it precedes the `/*---*/`
        // block), so the hashbang grammar (byte-0-only `#!`) could
        // never be exercised. Feed the original bytes unchanged.
        let outcome = run_script_with_fresh_runtime(
            exec,
            allow_blocking_atomics_wait,
            &source,
            &rel_path,
            test_path,
            features.iter().any(|feature| feature == "dynamic-import"),
        );
        invert_negative(outcome, frontmatter.negative.as_ref(), exec.timeout)
    } else {
        let stage_script = features.iter().any(|feature| feature == "dynamic-import");
        let mut ran_variant = false;
        if !frontmatter.is_only_strict() {
            ran_variant = true;
            let mut sloppy = String::with_capacity(preamble.len() + body.len());
            sloppy.push_str(&preamble);
            sloppy.push_str(body);
            let outcome = run_script_with_fresh_runtime(
                exec,
                allow_blocking_atomics_wait,
                &sloppy,
                &rel_path,
                test_path,
                stage_script,
            );
            let mapped = invert_negative(outcome, frontmatter.negative.as_ref(), exec.timeout);
            if !matches!(mapped, Outcome::Pass) {
                return result_with(
                    rel_path,
                    esid,
                    features,
                    label_variant_outcome("sloppy", mapped),
                    start,
                );
            }
        }
        if !frontmatter.is_no_strict() {
            ran_variant = true;
            let mut strict =
                String::with_capacity("\"use strict\";\n".len() + preamble.len() + body.len());
            strict.push_str("\"use strict\";\n");
            strict.push_str(&preamble);
            strict.push_str(body);
            let outcome = run_script_with_fresh_runtime(
                exec,
                allow_blocking_atomics_wait,
                &strict,
                &rel_path,
                test_path,
                stage_script,
            );
            let mapped = invert_negative(outcome, frontmatter.negative.as_ref(), exec.timeout);
            if !matches!(mapped, Outcome::Pass) {
                return result_with(
                    rel_path,
                    esid,
                    features,
                    label_variant_outcome("strict", mapped),
                    start,
                );
            }
        }
        if ran_variant {
            Outcome::Pass
        } else {
            Outcome::Skipped {
                feature: "no strictness variant".to_string(),
            }
        }
    };

    result_with(rel_path, esid, features, mapped, start)
}

fn run_script_with_fresh_runtime(
    exec: &ExecConfig,
    allow_blocking_atomics_wait: bool,
    source: &str,
    rel_path: &str,
    test_path: &Path,
    stage_on_disk: bool,
) -> Outcome {
    let mut runtime = match fresh_runtime(
        exec.timeout,
        exec.max_heap_bytes,
        allow_blocking_atomics_wait,
    ) {
        Ok(rt) => rt,
        Err(err) => {
            return Outcome::Crash {
                panic: format!("runtime construction failed: {err}"),
            };
        }
    };
    crate::agent::reset_for_next_test();
    run_script_test(
        &mut runtime,
        source,
        rel_path,
        test_path,
        stage_on_disk,
        exec.timeout,
    )
}

fn run_module_with_fresh_runtime(
    exec: &ExecConfig,
    allow_blocking_atomics_wait: bool,
    preamble: &str,
    body: &str,
    test_path: &Path,
) -> Outcome {
    let mut runtime = match fresh_runtime(
        exec.timeout,
        exec.max_heap_bytes,
        allow_blocking_atomics_wait,
    ) {
        Ok(rt) => rt,
        Err(err) => {
            return Outcome::Crash {
                panic: format!("runtime construction failed: {err}"),
            };
        }
    };
    crate::agent::reset_for_next_test();
    run_module_test(&mut runtime, preamble, body, test_path, exec.timeout)
}

fn label_variant_outcome(variant: &str, outcome: Outcome) -> Outcome {
    match outcome {
        Outcome::Fail { reason, stack } => Outcome::Fail {
            reason: format!("{variant}: {reason}"),
            stack,
        },
        Outcome::Crash { panic } => Outcome::Crash {
            panic: format!("{variant}: {panic}"),
        },
        Outcome::Timeout { ms } => Outcome::Timeout { ms },
        Outcome::OutOfMemory { bytes } => Outcome::OutOfMemory { bytes },
        Outcome::Skipped { feature } => Outcome::Skipped {
            feature: format!("{variant}: {feature}"),
        },
        Outcome::Pass => Outcome::Pass,
    }
}

fn run_script_test(
    runtime: &mut Runtime,
    source: &str,
    rel_path: &str,
    test_path: &Path,
    stage_on_disk: bool,
    timeout: Duration,
) -> Outcome {
    if stage_on_disk {
        let (dir, entry) = match stage_test_entry(source, test_path, "entry.js") {
            Ok(staged) => staged,
            Err(reason) => {
                return Outcome::Fail {
                    reason,
                    stack: None,
                };
            }
        };
        let outcome = run_with_watchdog(runtime, timeout, |rt| {
            let source = SourceInput::from_path(&entry)?;
            let specifier = file_url_for_path(&entry)?;
            rt.run_script(source, &specifier)
        });
        let mapped = map_watchdog_outcome(outcome);
        drop(dir);
        return mapped;
    }
    let outcome = run_with_watchdog(runtime, timeout, |rt| {
        rt.run_script(SourceInput::from_javascript(source.to_string()), rel_path)
    });
    map_watchdog_outcome(outcome)
}

fn run_module_test(
    runtime: &mut Runtime,
    preamble: &str,
    body: &str,
    test_path: &Path,
    timeout: Duration,
) -> Outcome {
    // INTERPRETING.md — harness files evaluate as classic scripts in
    // the global scope before the test, so `var assert` & friends are
    // genuine globals reachable from every module in the import
    // graph, not bindings local to the entry module's scope.
    if !preamble.is_empty() {
        let outcome = run_with_watchdog(runtime, timeout, |rt| {
            rt.run_script(
                SourceInput::from_javascript(preamble.to_string()),
                "test262-harness.js",
            )
        });
        if let mapped @ (Outcome::Fail { .. }
        | Outcome::Crash { .. }
        | Outcome::Timeout { .. }
        | Outcome::OutOfMemory { .. }) = map_watchdog_outcome(outcome)
        {
            return mapped;
        }
    }
    // Module entry must live on disk (the loader uses the parent
    // directory as the resolution base).
    let (dir, entry) = match stage_test_entry(body, test_path, "entry.mjs") {
        Ok(staged) => staged,
        Err(reason) => {
            return Outcome::Fail {
                reason,
                stack: None,
            };
        }
    };
    let outcome = run_with_watchdog(runtime, timeout, |rt| rt.run_module(&entry));
    let mapped = map_watchdog_outcome(outcome);
    drop(dir); // explicit; the temp dir auto-cleans on drop anyway
    mapped
}

fn stage_test_entry(
    source: &str,
    test_path: &Path,
    fallback_basename: &str,
) -> Result<(tempfile::TempDir, PathBuf), String> {
    let dir = match tempfile_dir() {
        Ok(dir) => dir,
        Err(err) => return Err(format!("tempdir creation failed: {err}")),
    };
    let basename = test_path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| std::ffi::OsString::from(fallback_basename));
    let entry = dir.path().join(&basename);
    if let Err(err) = std::fs::write(&entry, source) {
        return Err(format!("tempfile write failed: {err}"));
    }
    // Hard-link every sibling `.js` file from the test's source
    // directory into the temp dir so corpus-convention sibling
    // imports (`./<other>_FIXTURE.js`, sibling test files used in
    // ResolveExport / cycle / namespace-ambiguity coverage —
    // <https://tc39.es/ecma262/#sec-resolveexport>) resolve. Skip
    // the entry's own basename so the staged preamble+body wins.
    if let Some(parent) = test_path.parent()
        && let Ok(read_dir) = std::fs::read_dir(parent)
    {
        for sibling in read_dir.flatten() {
            let sibling_path = sibling.path();
            if !sibling_path.is_file() {
                continue;
            }
            let sibling_name = match sibling_path.file_name() {
                Some(n) => n.to_os_string(),
                None => continue,
            };
            if sibling_name == basename {
                continue;
            }
            let is_js = sibling_path.extension().and_then(std::ffi::OsStr::to_str) == Some("js");
            // Non-`.js` fixtures (JSON modules, extension-less
            // `with { type: "text" }` payloads) are part of the
            // corpus convention too.
            let is_fixture = sibling_name
                .to_str()
                .is_some_and(|name| name.contains("_FIXTURE"));
            if !is_js && !is_fixture {
                continue;
            }
            let dest = dir.path().join(&sibling_name);
            let _ = std::fs::hard_link(&sibling_path, &dest)
                .or_else(|_| std::fs::copy(&sibling_path, &dest).map(|_| ()));
        }
    }
    Ok((dir, entry))
}

fn tempfile_dir() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new().prefix("otter-test262-").tempdir()
}

fn file_url_for_path(path: &Path) -> Result<String, OtterError> {
    let canonical = std::fs::canonicalize(path).map_err(|err| OtterError::Io {
        path: path.to_path_buf(),
        kind: IoErrorKind::from_std(err.kind()),
        message: err.to_string(),
    })?;
    Ok(format!("file://{}", canonical.display()))
}

fn map_watchdog_outcome(outcome: WatchdogOutcome) -> Outcome {
    match outcome {
        WatchdogOutcome::Ok(_) => Outcome::Pass,
        WatchdogOutcome::Timeout { wall_ms } => Outcome::Timeout { ms: wall_ms },
        WatchdogOutcome::Panic(payload) => Outcome::Crash { panic: payload },
        WatchdogOutcome::Err(err) => map_otter_error(err),
    }
}

fn map_otter_error(err: OtterError) -> Outcome {
    match err {
        OtterError::Interrupted => Outcome::Timeout { ms: 0 },
        OtterError::OutOfMemory {
            requested_bytes, ..
        } => Outcome::OutOfMemory {
            bytes: requested_bytes,
        },
        OtterError::Timeout { elapsed_ms } => Outcome::Timeout { ms: elapsed_ms },
        OtterError::Compile { diagnostics } => Outcome::Fail {
            reason: render_compile_diagnostics(&diagnostics),
            stack: None,
        },
        OtterError::Runtime { diagnostic } => Outcome::Fail {
            reason: render_runtime_diagnostic(&diagnostic),
            stack: render_stack(&diagnostic),
        },
        OtterError::Internal { code, message } => Outcome::Crash {
            panic: format!("engine internal error ({code}): {message}"),
        },
        other => Outcome::Fail {
            reason: format!("unmapped engine error: {other}"),
            stack: None,
        },
    }
}

fn invert_negative(
    outcome: Outcome,
    negative: Option<&crate::metadata::Negative>,
    _timeout: Duration,
) -> Outcome {
    let Some(negative) = negative else {
        return outcome;
    };
    let phase = negative.phase.canonical();
    let want_type = negative.type_.as_str();
    match (&outcome, phase) {
        // Spec-correct: parse-phase test threw at compile time.
        (Outcome::Fail { reason, stack: _ }, NegativePhase::Parse)
            if reason.starts_with("compile:") =>
        {
            // Compile diagnostic carries the wanted type when the
            // engine can identify it; otherwise we accept any
            // compile failure for `phase: parse`.
            if reason_carries_type(reason, want_type) || want_type == "SyntaxError" {
                Outcome::Pass
            } else {
                Outcome::Fail {
                    reason: format!("negative phase=parse expected {want_type}, got: {reason}"),
                    stack: None,
                }
            }
        }
        (Outcome::Fail { reason, stack: _ }, NegativePhase::Resolution)
            if reason.starts_with("compile:") =>
        {
            // Foundation: linker/resolution errors surface as
            // compile diagnostics. Accept any compile failure for
            // `phase: resolution`.
            if reason_carries_type(reason, want_type) || want_type == "SyntaxError" {
                Outcome::Pass
            } else {
                Outcome::Fail {
                    reason: format!(
                        "negative phase=resolution expected {want_type}, got: {reason}"
                    ),
                    stack: None,
                }
            }
        }
        (Outcome::Fail { reason, stack: _ }, NegativePhase::Runtime)
            if reason.starts_with("runtime:") =>
        {
            if reason_carries_type(reason, want_type) {
                Outcome::Pass
            } else {
                Outcome::Fail {
                    reason: format!("negative phase=runtime expected {want_type}, got: {reason}"),
                    stack: None,
                }
            }
        }
        // Negative test that returned normally (no error).
        (Outcome::Pass, _) => Outcome::Fail {
            reason: format!(
                "negative phase={phase:?} expected {want_type} but execution completed normally"
            ),
            stack: None,
        },
        // Pass-through for skipped / crashed / timeout / OOM —
        // these are *not* spec-equivalent successes.
        (other, _) => other.clone(),
    }
}

fn reason_carries_type(reason: &str, want_type: &str) -> bool {
    // Engine renders the diagnostic with the JS error name in the
    // message. Substring match is sufficient — spec-correct
    // matchers can refine later.
    reason.contains(want_type)
}

fn render_compile_diagnostics(diags: &[Diagnostic]) -> String {
    let codes: Vec<String> = diags.iter().map(|d| d.code.clone()).collect();
    let messages: Vec<String> = diags.iter().map(|d| d.message.clone()).collect();
    format!(
        "compile: codes=[{}] messages=[{}]",
        codes.join(", "),
        messages.join(" | ")
    )
}

fn render_runtime_diagnostic(diag: &Diagnostic) -> String {
    let kind_label = match diag.kind {
        DiagnosticKind::Type => "TypeError",
        DiagnosticKind::Reference => "ReferenceError",
        DiagnosticKind::Range => "RangeError",
        DiagnosticKind::Syntax => "SyntaxError",
        DiagnosticKind::OutOfMemory => "RangeError",
        DiagnosticKind::Timeout => "Timeout",
        DiagnosticKind::Capability => "CapabilityError",
        DiagnosticKind::Internal => "InternalError",
        // `DiagnosticKind` is `#[non_exhaustive]`; treat any
        // future variants as a generic Error so the runner
        // remains forward-compatible.
        _ => "Error",
    };
    format!("runtime: {kind_label} ({}) {}", diag.code, diag.message)
}

fn render_stack(diag: &Diagnostic) -> Option<String> {
    if diag.frames.is_empty() {
        return None;
    }
    let joined: Vec<String> = diag
        .frames
        .iter()
        .map(|f| format!("  at {} ({})", f.function, f.module))
        .collect();
    Some(joined.join("\n"))
}

fn relative_path(test_path: &Path, test_dir: &Path) -> String {
    test_path
        .strip_prefix(test_dir)
        .unwrap_or(test_path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn result_with(
    path: String,
    esid: Option<String>,
    features: Vec<String>,
    outcome: Outcome,
    start: std::time::Instant,
) -> TestResult {
    TestResult {
        path,
        esid,
        features,
        outcome,
        wall_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

/// Errors raised by [`ensure_corpus_present`].
#[derive(Debug, Error)]
pub enum CorpusError {
    /// Submodule directory is missing entirely.
    #[error(
        "vendor/test262 is missing at {root:?}. Run: git submodule update --init --recursive vendor/test262"
    )]
    Missing {
        /// The expected submodule root.
        root: PathBuf,
    },
    /// Submodule is present but empty (uninitialised).
    #[error(
        "vendor/test262 is empty at {root:?} — the submodule is not initialised. Run: git submodule update --init --recursive vendor/test262"
    )]
    Empty {
        /// The submodule root.
        root: PathBuf,
    },
    /// I/O error while walking the corpus.
    #[error("io error reading {path:?}: {message}")]
    Io {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying error message.
        message: String,
    },
}
