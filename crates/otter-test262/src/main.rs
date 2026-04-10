//! Test262 runner for the **new** Otter VM (`otter-runtime` / `otter-vm`).
//!
//! Phase 4 of the bootstrap plan. Reuses the existing metadata parser and
//! report infrastructure but executes tests through `OtterRuntime`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use colored::*;
use indicatif::{ProgressBar, ProgressStyle};
use walkdir::WalkDir;

use otter_runtime::{
    NativeFunctionDescriptor, OtterRuntime, RegisterValue, RunError, RuntimeState,
    VmNativeCallError,
};
use otter_test262::config::Test262Config;
use otter_test262::metadata::{ExecutionMode, TestMetadata};
use otter_test262::{RunSummary, TestOutcome, TestResult};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "test262")]
#[command(about = "Run Test262 conformance tests against the Otter VM")]
struct Cli {
    /// Path to test262 directory
    #[arg(short, long, default_value = "tests/test262")]
    test_dir: PathBuf,

    /// Path to the TOML config file (default: `test262_config.toml` in the
    /// current working directory). The config provides the default list of
    /// skipped features, ignored test-path patterns, and known-panic
    /// patterns. CLI flags still take precedence over values it supplies.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Filter tests by path pattern
    #[arg(short, long)]
    filter: Option<String>,

    /// Run only tests in this subdirectory (e.g., "built-ins/Math")
    #[arg(short = 'd', long)]
    subdir: Option<String>,

    /// Verbosity: -v colored dots, -vv names, -vvv output
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Maximum number of tests to run
    #[arg(short = 'n', long)]
    max_tests: Option<usize>,

    /// Timeout per test in milliseconds
    #[arg(long, default_value = "5000")]
    timeout: u64,

    /// Path for JSONL result log (one TestResult per line — streaming).
    #[arg(long)]
    log: Option<PathBuf>,

    /// Save a canonical `PersistedReport` JSON (summary + all results) to
    /// the given path at the end of the run. This is the format consumed
    /// by `gen-conformance` and by the batch-merge tool.
    #[arg(long)]
    save: Option<PathBuf>,

    /// Features to skip (comma-separated)
    #[arg(long, value_delimiter = ',')]
    skip_features: Vec<String>,

    /// Hard heap cap per test in bytes. Protects against pathological
    /// Array tests (e.g. `new Array(2**32-1)`) that would otherwise OOM
    /// the host. Default: 512 MB. Pass `0` to disable the cap.
    #[arg(long, default_value = "536870912")]
    max_heap_bytes: usize,

    /// Enable memory-leak profiling — takes a heap snapshot every
    /// `--memory-profile-interval` tests and reports the top growing
    /// types. Intended for diagnosing leaks, not for every run.
    #[arg(long)]
    memory_profile: bool,

    /// Interval (in tests) between memory profile snapshots. Defaults
    /// to 100 so the O(N) heap walk happens at most 1% of the time.
    #[arg(long, default_value = "100")]
    memory_profile_interval: usize,
}

// ---------------------------------------------------------------------------
// Shared harness state — thread-local for zero-overhead in fn-ptr callbacks
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct HarnessInner {
    print_output: Vec<String>,
    done_result: Option<Result<(), String>>,
}

thread_local! {
    static HARNESS_STATE: std::cell::RefCell<HarnessInner> =
        std::cell::RefCell::new(HarnessInner::default());
}

fn harness_clear() {
    HARNESS_STATE.with(|cell| {
        let mut inner = cell.borrow_mut();
        inner.print_output.clear();
        inner.done_result = None;
    });
}

// ---------------------------------------------------------------------------
// Native callbacks (fn pointers — work with VmNativeFunction ABI)
// ---------------------------------------------------------------------------

fn test262_print(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    HARNESS_STATE.with(|cell| {
        let mut inner = cell.borrow_mut();
        for arg in args {
            inner
                .print_output
                .push(format_register_value(*arg, runtime));
        }
    });
    Ok(RegisterValue::undefined())
}

fn test262_done(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    HARNESS_STATE.with(|cell| {
        let mut inner = cell.borrow_mut();
        if let Some(arg) = args.first()
            && *arg != RegisterValue::undefined()
            && *arg != RegisterValue::null()
            && arg.as_bool() != Some(false)
        {
            inner.done_result = Some(Err(format_register_value(*arg, runtime)));
            return;
        }
        inner.done_result = Some(Ok(()));
    });
    Ok(RegisterValue::undefined())
}

/// `$262.createRealm()` — bootstraps a brand-new ECMAScript realm with its
/// own intrinsics and global object, and exposes a `{ global }` wrapper in
/// the **current** realm that test262 harness code uses to reach into it.
fn test262_create_realm(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // §9.3.3 — fresh realm with its own VmIntrinsics, prototypes, and globals.
    let new_realm_id = runtime.create_realm().map_err(|error| {
        VmNativeCallError::Internal(
            format!("$262.createRealm create_realm failed: {error:?}").into(),
        )
    })?;
    let realm_global = runtime.realm(new_realm_id).intrinsics.global_object();

    // The wrapper object lives in the *caller*'s realm; only `.global` is
    // cross-realm. test262 harness uses `other = $262.createRealm().global`.
    let realm = runtime.alloc_object();
    let global_property = runtime.intern_property_name("global");
    runtime
        .objects_mut()
        .set_property(
            realm,
            global_property,
            RegisterValue::from_object_handle(realm_global.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("$262.createRealm global install failed: {error:?}").into(),
            )
        })?;
    Ok(RegisterValue::from_object_handle(realm.0))
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

struct NewVmRunner {
    test_dir: PathBuf,
    filter: Option<String>,
    skip_features: HashSet<String>,
    /// Test-path substrings (from `test262_config.toml` `ignored_tests`)
    /// whose matching tests are reported as `Skip` with reason
    /// "ignored by config". This matches the ad-hoc CLI skipping that was
    /// previously necessary and keeps pass-rate numbers honest.
    ignored_test_patterns: Vec<String>,
    /// Known-panic test-path substrings. Tests matching one of these are
    /// reported as `Skip` with reason "known panic". The intent is to keep
    /// the runner moving forward while the underlying crash is fixed.
    known_panic_patterns: Vec<String>,
    harness_cache: HashMap<String, String>,
    timeout: Duration,
    /// Hard heap cap passed to every `OtterRuntime::builder().max_heap_bytes`
    /// call. `0` disables the cap.
    max_heap_bytes: usize,
    memory_profile: bool,
    memory_profile_interval: usize,
}

impl NewVmRunner {
    fn new(test_dir: &Path, timeout: Duration, max_heap_bytes: usize) -> Self {
        let harness_dir = test_dir.join("harness");
        let mut harness_cache = HashMap::new();

        if harness_dir.is_dir() {
            for entry in fs::read_dir(&harness_dir).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".js")
                    && let Ok(content) = fs::read_to_string(entry.path())
                {
                    harness_cache.insert(name, content);
                }
            }
        }

        Self {
            test_dir: test_dir.to_path_buf(),
            filter: None,
            skip_features: HashSet::new(),
            ignored_test_patterns: Vec::new(),
            known_panic_patterns: Vec::new(),
            harness_cache,
            timeout,
            max_heap_bytes,
            memory_profile: false,
            memory_profile_interval: 100,
        }
    }

    fn with_memory_profile(mut self, enabled: bool, interval: usize) -> Self {
        self.memory_profile = enabled;
        self.memory_profile_interval = interval.max(1);
        self
    }

    fn with_filter(mut self, filter: String) -> Self {
        self.filter = Some(filter);
        self
    }

    fn with_skip_features(mut self, features: Vec<String>) -> Self {
        self.skip_features = features.into_iter().collect();
        self
    }

    fn with_ignored_tests(mut self, patterns: Vec<String>) -> Self {
        self.ignored_test_patterns = patterns;
        self
    }

    fn with_known_panics(mut self, patterns: Vec<String>) -> Self {
        self.known_panic_patterns = patterns;
        self
    }

    /// Returns Some(reason) if the given normalized test path matches one
    /// of the configured ignore patterns. The reason is the matched pattern
    /// itself — both for user visibility and so the summary can group the
    /// skips by category.
    fn ignored_reason(&self, path: &str) -> Option<&str> {
        self.ignored_test_patterns
            .iter()
            .find(|p| path.contains(p.as_str()))
            .map(String::as_str)
    }

    fn known_panic_reason(&self, path: &str) -> Option<&str> {
        self.known_panic_patterns
            .iter()
            .find(|p| path.contains(p.as_str()))
            .map(String::as_str)
    }

    fn list_tests(&self, subdir: Option<&str>) -> Vec<PathBuf> {
        let base = match subdir {
            Some(sub) => self.test_dir.join("test").join(sub),
            None => self.test_dir.join("test"),
        };

        if !base.exists() {
            eprintln!("Test directory does not exist: {}", base.display());
            return Vec::new();
        }

        let mut tests: Vec<PathBuf> = WalkDir::new(&base)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension().is_some_and(|ext| ext == "js")
                    && !e.path().to_string_lossy().contains("_FIXTURE")
            })
            .map(|e| e.into_path())
            .collect();

        if let Some(ref filter) = self.filter {
            tests.retain(|p| p.to_string_lossy().contains(filter.as_str()));
        }

        tests.sort();
        tests
    }

    /// Install test262 harness globals on the runtime.
    fn setup_harness(rt: &mut OtterRuntime) {
        rt.state_mut()
            .install_native_global(NativeFunctionDescriptor::method("print", 1, test262_print));
        rt.state_mut()
            .install_native_global(NativeFunctionDescriptor::method("$DONE", 1, test262_done));

        let state = rt.state_mut();
        let create_realm = state.register_native_function(NativeFunctionDescriptor::method(
            "createRealm",
            0,
            test262_create_realm,
        ));
        let create_realm = state.alloc_host_function(create_realm);
        let test262 = state.alloc_object();
        let global = state.intrinsics().global_object();
        let global_property = state.intern_property_name("global");
        state
            .objects_mut()
            .set_property(
                test262,
                global_property,
                RegisterValue::from_object_handle(global.0),
            )
            .expect("$262.global should install");
        let create_realm_property = state.intern_property_name("createRealm");
        state
            .objects_mut()
            .set_property(
                test262,
                create_realm_property,
                RegisterValue::from_object_handle(create_realm.0),
            )
            .expect("$262.createRealm should install");
        state.install_global_value("$262", RegisterValue::from_object_handle(test262.0));
    }

    fn run_test_all_modes(&self, path: &Path) -> Vec<TestResult> {
        let relative_path = path.strip_prefix(&self.test_dir).unwrap_or(path);
        let relative_path_str = relative_path.to_string_lossy().replace('\\', "/");

        // Config-driven skips: the list lives in `test262_config.toml` so
        // users can update it without touching Rust. Apply BEFORE reading
        // the file so known-panic / ignored patterns skip cheaply without
        // risking a crash on disk I/O paths we already know are broken.
        if let Some(reason) = self.known_panic_reason(&relative_path_str) {
            return vec![TestResult {
                path: relative_path_str,
                mode: ExecutionMode::NonStrict,
                outcome: TestOutcome::Skip,
                duration_ms: 0,
                error: Some(format!("Known panic: {reason}")),
                features: vec![],
            }];
        }
        if let Some(reason) = self.ignored_reason(&relative_path_str) {
            return vec![TestResult {
                path: relative_path_str,
                mode: ExecutionMode::NonStrict,
                outcome: TestOutcome::Skip,
                duration_ms: 0,
                error: Some(format!("Ignored by config: {reason}")),
                features: vec![],
            }];
        }

        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                return vec![TestResult {
                    path: path.to_string_lossy().to_string(),
                    mode: ExecutionMode::NonStrict,
                    outcome: TestOutcome::Crash,
                    duration_ms: 0,
                    error: Some(format!("Failed to read file: {e}")),
                    features: vec![],
                }];
            }
        };

        let metadata = match TestMetadata::parse(&content) {
            Some(m) => m,
            None => {
                return vec![TestResult {
                    path: path.to_string_lossy().to_string(),
                    mode: ExecutionMode::NonStrict,
                    outcome: TestOutcome::Crash,
                    duration_ms: 0,
                    error: Some("Failed to parse test metadata".to_string()),
                    features: vec![],
                }];
            }
        };

        // Skip features.
        for feature in &metadata.features {
            if self.skip_features.contains(feature) {
                return vec![TestResult {
                    path: relative_path_str,
                    mode: ExecutionMode::NonStrict,
                    outcome: TestOutcome::Skip,
                    duration_ms: 0,
                    error: Some(format!("Skipped feature: {feature}")),
                    features: metadata.features.clone(),
                }];
            }
        }

        let modes = metadata.execution_modes();
        let mut results = Vec::with_capacity(modes.len());

        for mode in &modes {
            let start = Instant::now();
            let (outcome, error) =
                self.run_single_mode(&content, &metadata, *mode, &relative_path_str);
            results.push(TestResult {
                path: relative_path_str.clone(),
                mode: *mode,
                outcome,
                duration_ms: start.elapsed().as_millis() as u64,
                error,
                features: metadata.features.clone(),
            });
        }

        results
    }

    fn run_single_mode(
        &self,
        content: &str,
        metadata: &TestMetadata,
        mode: ExecutionMode,
        source_url: &str,
    ) -> (TestOutcome, Option<String>) {
        if mode == ExecutionMode::Module {
            return (
                TestOutcome::Skip,
                Some("Module mode not yet supported".to_string()),
            );
        }

        let test_body = content
            .find("---*/")
            .map(|i| &content[i + 5..])
            .unwrap_or(content);

        // Fresh runtime per test.
        harness_clear();
        let mut rt = OtterRuntime::builder()
            .timeout(self.timeout)
            .max_heap_bytes(self.max_heap_bytes)
            .build();
        Self::setup_harness(&mut rt);

        if !metadata.is_raw() {
            let includes = self.harness_includes(metadata);
            for include in includes {
                let Some(harness_content) = self.harness_cache.get(&include) else {
                    return (
                        TestOutcome::Fail,
                        Some(format!("Harness file '{include}' not found in cache")),
                    );
                };
                let harness_source = Self::script_source_for_mode(harness_content, mode);
                let harness_url = format!("{source_url}::{include}");
                match rt.run_script(&harness_source, &harness_url) {
                    Ok(_) => {}
                    Err(RunError::Compile(e)) => {
                        return (
                            TestOutcome::Fail,
                            Some(format!("Harness compile error in {include}: {e}")),
                        );
                    }
                    Err(RunError::Runtime(e)) => {
                        return (
                            if e.contains("execution interrupted") {
                                TestOutcome::Timeout
                            } else {
                                TestOutcome::Fail
                            },
                            Some(format!("Harness runtime error in {include}: {e}")),
                        );
                    }
                    Err(RunError::JsThrow(diag)) => {
                        // Render the V8-style stack via the diagnostic so
                        // that `-vv` keeps the structured frame info.
                        return (
                            TestOutcome::Fail,
                            Some(format!(
                                "Harness runtime error in {include}: {}",
                                diag.rendered_stack()
                            )),
                        );
                    }
                }
            }
        }

        let test_source = Self::script_source_for_mode(test_body, mode);

        match rt.run_script(&test_source, source_url) {
            Ok(_) => {
                if metadata.expects_early_error() || metadata.expects_runtime_error() {
                    (
                        TestOutcome::Fail,
                        Some("Expected error but execution succeeded".to_string()),
                    )
                } else {
                    (TestOutcome::Pass, None)
                }
            }
            Err(RunError::Compile(e)) => {
                if metadata.expects_early_error() {
                    (TestOutcome::Pass, None)
                } else {
                    (TestOutcome::Fail, Some(format!("Compile error: {e}")))
                }
            }
            Err(RunError::Runtime(e)) => {
                if e.contains("execution interrupted") {
                    (TestOutcome::Timeout, Some(e))
                } else if e.contains("out of memory") {
                    // Heap-cap violation: tracked as a distinct outcome so
                    // conformance reports can tell pathological Array tests
                    // apart from real VM crashes.
                    (TestOutcome::OutOfMemory, Some(e))
                } else if metadata.expects_runtime_error() {
                    (TestOutcome::Pass, None)
                } else {
                    (TestOutcome::Fail, Some(format!("RuntimeError: {e}")))
                }
            }
            Err(RunError::JsThrow(diag)) => {
                // Spec-required error path: tests like
                // `built-ins/Error/prototype/toString/tostring-get-throws.js`
                // ARE allowed to throw runtime errors. Treat the structured
                // diagnostic the same as the legacy runtime error variant.
                if metadata.expects_runtime_error() {
                    (TestOutcome::Pass, None)
                } else {
                    (
                        TestOutcome::Fail,
                        Some(format!("RuntimeError: {}", diag.rendered_stack())),
                    )
                }
            }
        }
    }

    fn harness_includes(&self, metadata: &TestMetadata) -> Vec<String> {
        let mut includes = vec!["sta.js".to_string(), "assert.js".to_string()];
        if metadata.is_async() {
            includes.push("doneprintHandle.js".to_string());
        }
        for include in &metadata.includes {
            if !includes.contains(include) {
                includes.push(include.clone());
            }
        }
        includes
    }

    fn script_source_for_mode(source: &str, mode: ExecutionMode) -> String {
        if mode == ExecutionMode::Strict {
            let mut prefixed = String::with_capacity(source.len() + 16);
            prefixed.push_str("\"use strict\";\n");
            prefixed.push_str(source);
            prefixed
        } else {
            source.to_string()
        }
    }
}

fn format_register_value(value: RegisterValue, runtime: &mut RuntimeState) -> String {
    // ES spec §7.1.17 ToString — for Error objects this invokes
    // Error.prototype.toString (§20.5.3.4) which returns "Name: Message".
    runtime.js_to_string_infallible(value).to_string()
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

/// Build a fresh runtime, install the test262 harness, and snapshot its
/// heap stats. The returned value is the `HeapTypeStats` "starting line"
/// for an unused runtime — any drift of this baseline across tests is a
/// leak signal in thread-local or process-global state.
fn probe_heap_baseline(runner: &NewVmRunner) -> otter_runtime::HeapTypeStats {
    let mut rt = OtterRuntime::builder()
        .timeout(runner.timeout)
        .max_heap_bytes(runner.max_heap_bytes)
        .build();
    NewVmRunner::setup_harness(&mut rt);
    rt.state().collect_heap_stats()
}

/// Render a human-readable delta between two `HeapTypeStats` snapshots.
/// Only the top 10 growing variants by byte delta are shown, which keeps
/// the output line count bounded during a long run.
fn print_heap_diff(
    baseline: &otter_runtime::HeapTypeStats,
    current: &otter_runtime::HeapTypeStats,
    test_index: usize,
) {
    let count_delta = current.total_count as i64 - baseline.total_count as i64;
    let bytes_delta = current.total_bytes as i64 - baseline.total_bytes as i64;
    eprintln!(
        "[memory] test={test_index} total: count {:+} ({}), bytes {:+} ({})",
        count_delta, current.total_count, bytes_delta, current.total_bytes
    );

    let mut deltas: Vec<(&'static str, i64, i64)> = Vec::new();
    for (name, (count, bytes)) in &current.by_type {
        let (base_count, base_bytes) = baseline.by_type.get(name).copied().unwrap_or_default();
        let dc = *count as i64 - base_count as i64;
        let db = *bytes as i64 - base_bytes as i64;
        if dc != 0 || db != 0 {
            deltas.push((name, dc, db));
        }
    }
    // Sort by byte delta, largest first.
    deltas.sort_by(|a, b| b.2.cmp(&a.2));
    for (name, dc, db) in deltas.iter().take(10) {
        if *dc == 0 && *db == 0 {
            continue;
        }
        eprintln!("  {name:<24} count {dc:+6} bytes {db:+10}");
    }
}

fn main() {
    let cli = Cli::parse();

    eprintln!("{}", "Otter Test262 Runner".bold().cyan());
    eprintln!("Test directory: {}", cli.test_dir.display());

    // Load `test262_config.toml` (or the path the user gave via --config).
    // CLI flags remain authoritative — the config only provides defaults
    // the user can override.
    let config = Test262Config::load_or_default(cli.config.as_deref());
    if !config.skip_features.is_empty()
        || !config.ignored_tests.is_empty()
        || !config.known_panics.is_empty()
    {
        eprintln!(
            "Config loaded: skip_features={}, ignored_tests={}, known_panics={}",
            config.skip_features.len(),
            config.ignored_tests.len(),
            config.known_panics.len(),
        );
    }

    // Merge CLI `--skip-features` (higher priority) with config defaults.
    let mut merged_skip_features: Vec<String> = config.skip_features.clone();
    for feat in &cli.skip_features {
        if !merged_skip_features.contains(feat) {
            merged_skip_features.push(feat.clone());
        }
    }

    // Resolve timeout: CLI flag wins, then config `timeout_secs`, then
    // the clap default (5000 ms).
    let timeout_ms = if cli.timeout != 5000 {
        cli.timeout
    } else if let Some(secs) = config.timeout_secs {
        secs.saturating_mul(1000)
    } else {
        cli.timeout
    };

    // Resolve heap cap the same way.
    let max_heap_bytes = if cli.max_heap_bytes != 536_870_912 {
        cli.max_heap_bytes
    } else if let Some(bytes) = config.max_heap_bytes_per_test {
        bytes
    } else {
        cli.max_heap_bytes
    };

    let mut runner = NewVmRunner::new(
        &cli.test_dir,
        Duration::from_millis(timeout_ms),
        max_heap_bytes,
    )
    .with_memory_profile(cli.memory_profile, cli.memory_profile_interval)
    .with_ignored_tests(config.ignored_tests.clone())
    .with_known_panics(config.known_panics.clone());

    if max_heap_bytes > 0 {
        eprintln!(
            "Max heap bytes per test: {} ({} MB)",
            max_heap_bytes,
            max_heap_bytes / (1024 * 1024)
        );
    } else {
        eprintln!("Max heap bytes per test: unlimited");
    }

    if let Some(ref filter) = cli.filter {
        runner = runner.with_filter(filter.clone());
        eprintln!("Filter: {filter}");
    }
    if !merged_skip_features.is_empty() {
        eprintln!("Skipping features: {} total", merged_skip_features.len());
        runner = runner.with_skip_features(merged_skip_features);
    }

    let mut tests = runner.list_tests(cli.subdir.as_deref());

    if let Some(max) = cli.max_tests {
        tests.truncate(max);
    }

    let test_count = tests.len();
    eprintln!("Found {} test files", test_count);

    let show_progress = cli.verbose < 2;
    let pb = if show_progress {
        let pb = ProgressBar::new(test_count as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) | {msg}")
                .unwrap()
                .progress_chars("#>-"),
        );
        pb.set_message("starting...");
        Some(pb)
    } else {
        None
    };

    let mut log_writer: Option<std::io::BufWriter<fs::File>> = cli.log.as_ref().and_then(|p| {
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(p)
            .ok()
            .map(std::io::BufWriter::new)
    });

    let mut summary = RunSummary::new(10000);
    let run_start = Instant::now();

    // Memory-profile baseline (captured from the first probed runtime).
    // The runner creates a fresh `OtterRuntime` for every test, so the
    // interesting leak signal is *baseline drift* across identical
    // harness-setup runtimes — drift indicates thread-local/static state
    // accumulating between drops (exactly what the new-vm drop+jit cleanup
    // path guards against).
    let mut memory_baseline: Option<otter_runtime::HeapTypeStats> = None;
    let memory_profile = runner.memory_profile;
    let memory_profile_interval = runner.memory_profile_interval;
    if memory_profile {
        eprintln!("Memory profile enabled (interval: every {memory_profile_interval} tests)");
    }

    for (test_index, path) in tests.iter().enumerate() {
        if memory_profile && test_index.is_multiple_of(memory_profile_interval) {
            let stats = probe_heap_baseline(&runner);
            if let Some(ref baseline) = memory_baseline {
                print_heap_diff(baseline, &stats, test_index);
            } else {
                eprintln!(
                    "[memory] baseline at test {test_index}: total_count={}, total_bytes={}",
                    stats.total_count, stats.total_bytes
                );
                memory_baseline = Some(stats);
            }
        }
        let results = runner.run_test_all_modes(path);

        for result in &results {
            match cli.verbose {
                0 => {}
                1 => {
                    let ch = match result.outcome {
                        TestOutcome::Pass => ".".green(),
                        TestOutcome::Fail => "F".red(),
                        TestOutcome::Skip => "S".yellow(),
                        TestOutcome::Timeout => "T".magenta(),
                        TestOutcome::Crash => "!".red().bold(),
                        TestOutcome::OutOfMemory => "M".red().bold(),
                    };
                    eprint!("{ch}");
                    if summary.total % 80 == 79 {
                        eprintln!();
                    }
                }
                _ => {
                    let status = match result.outcome {
                        TestOutcome::Pass => "PASS".green(),
                        TestOutcome::Fail => "FAIL".red(),
                        TestOutcome::Skip => "SKIP".yellow(),
                        TestOutcome::Timeout => "TIME".magenta(),
                        TestOutcome::Crash => "CRASH".red().bold(),
                        TestOutcome::OutOfMemory => "OOM".red().bold(),
                    };
                    eprintln!("[{status}] {} ({})", result.path, result.mode);
                    if let Some(ref err) = result.error {
                        eprintln!("  {err}");
                    }
                }
            }

            if let Some(ref mut writer) = log_writer
                && let Ok(line) = serde_json::to_string(result)
            {
                let _ = writeln!(writer, "{line}");
            }

            // `--save` writes a full `PersistedReport` at the end of the
            // run, so we need to keep every individual `TestResult`.
            summary.record(result, cli.save.is_some());
        }

        if let Some(ref pb) = pb {
            pb.inc(1);
            pb.set_message(format!(
                "Pass: {} Fail: {} Skip: {}",
                summary.passed, summary.failed, summary.skipped
            ));
        }
    }

    if let Some(ref mut writer) = log_writer {
        let _ = writer.flush();
    }

    if let Some(ref pb) = pb {
        pb.finish_and_clear();
    }
    if cli.verbose == 1 {
        eprintln!();
    }

    let duration = run_start.elapsed();
    let save_path = cli.save.clone();
    let all_results = if save_path.is_some() {
        std::mem::take(&mut summary.all_results)
    } else {
        Vec::new()
    };
    let report = summary.into_report();
    report.print_summary();
    eprintln!("Duration: {:.1}s", duration.as_secs_f64());

    if let Some(path) = save_path {
        let persisted = otter_test262::PersistedReport {
            timestamp: chrono::Utc::now().to_rfc3339(),
            otter_version: env!("CARGO_PKG_VERSION").to_string(),
            test262_commit: None,
            duration_secs: duration.as_secs_f64(),
            summary: report,
            results: all_results,
        };
        match persisted.save(&path) {
            Ok(()) => eprintln!("Saved PersistedReport to {}", path.display()),
            Err(e) => eprintln!("Warning: failed to save report to {}: {e}", path.display()),
        }
    }
}
