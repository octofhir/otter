//! Node.js compatibility runner over the official test corpus.
//!
//! # Contents
//! - Full-corpus execution of the vendored `test/parallel` and
//!   `test/sequential` suites, grouped into modules by Node's own test naming.
//! - Configured integration commands for compatibility surfaces that need
//!   build fixtures, such as native Node-API addons.
//! - Watchdog execution and generated JSON/Markdown/dashboard conformance
//!   reports.
//!
//! # Invariants
//! - Every vendored test file runs; the corpus is never pre-filtered, so the
//!   report always states the true number of unsupported behaviours.
//! - The default tested Otter binary is always rebuilt in release mode.
//! - Every child runs in its own process group and is terminated by the
//!   external watchdog on timeout.
//! - Configured commands use owned strings and never cross VM/GC boundaries.
//! - `test/sequential` files and configured commands run one at a time and
//!   never overlap the concurrent phase; only `test/parallel` files, which Node
//!   itself declares safe to run side by side, are spread across workers.
//! - Every child receives the `TEST_SERIAL_ID` and `TEST_PARALLEL` variables
//!   Node's own harness reads, so concurrent tests get private temporary
//!   directories and a `common.PORT` access is reported instead of silently
//!   binding a shared port.
//! - Results are reported in corpus order regardless of completion order, so
//!   the generated documents diff cleanly between runs.
//!
//! # See also
//! - `node_compat_config.toml`
//! - `NODE_CONFORMANCE.md`
//! - `docs/site/src/content/docs/conformance/nodejs.mdx`

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct NodeCompatConfig {
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Per-module timeout overrides for modules whose tests are legitimately
    /// slower than the global budget. Never used to exclude tests.
    #[serde(default)]
    pub module_timeouts: BTreeMap<String, u64>,
    #[serde(default)]
    pub integration_tests: Vec<NodeCompatIntegrationTest>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeCompatIntegrationTest {
    pub module: String,
    pub name: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub workspace_root: PathBuf,
    pub config_path: PathBuf,
    pub selected_modules: Vec<String>,
    pub limit: Option<usize>,
    pub substring_filter: Option<String>,
    pub timeout_secs: Option<u64>,
    pub otter_bin: Option<PathBuf>,
    /// Worker count for the `test/parallel` phase. `None` derives it from the
    /// machine, leaving headroom so the host stays responsive during a run.
    pub jobs: Option<usize>,
}

impl RunOptions {
    /// A run that narrows the corpus measures a slice, not conformance, so its
    /// numbers must never reach the published baseline.
    #[must_use]
    pub fn is_full_corpus(&self) -> bool {
        self.selected_modules.is_empty() && self.substring_filter.is_none() && self.limit.is_none()
    }

    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            config_path: workspace_root.join("node_compat_config.toml"),
            workspace_root,
            selected_modules: Vec::new(),
            limit: None,
            substring_filter: None,
            timeout_secs: None,
            otter_bin: None,
            jobs: None,
        }
    }

    /// Worker count for the concurrent phase. Two cores are held back: one for
    /// the runner's own bookkeeping and one so the machine stays usable, which
    /// also keeps timing-sensitive Node tests off a fully saturated scheduler.
    ///
    /// The ceiling is not advisory. A single worker is a whole process tree —
    /// the test, whatever it spawns, and whatever those spawn in turn — so a
    /// width chosen for cores alone can put a machine under hundreds of live
    /// processes. `--jobs` may lower this number and cannot raise it past
    /// [`MAX_CONCURRENT_TESTS`].
    fn worker_count(&self) -> usize {
        self.jobs
            .unwrap_or_else(|| {
                thread::available_parallelism()
                    .map(|cores| cores.get().saturating_sub(2))
                    .unwrap_or(1)
            })
            .clamp(1, MAX_CONCURRENT_TESTS)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub timestamp: DateTime<Utc>,
    pub otter_version: String,
    pub otter_binary: String,
    /// `nodejs/node` commit the vendored corpus was checked out from; every
    /// reported test links back to its source at this revision.
    pub node_commit: String,
    pub engine_commit: String,
    pub duration_secs: f64,
    pub summary: RunSummary,
    pub results: Vec<TestResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub timeout: usize,
    pub crashed: usize,
    pub pass_rate: f64,
    pub by_module: BTreeMap<String, ModuleSummary>,
    pub failures: Vec<FailureSummary>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ModuleSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub timeout: usize,
    pub crashed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailureSummary {
    pub path: String,
    pub module: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub path: String,
    pub module: String,
    pub outcome: Outcome,
    pub duration_ms: u128,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail,
    Skipped,
    Timeout,
    Crashed,
}

#[derive(Debug, Clone)]
struct PlannedTest {
    module: String,
    display_path: String,
    kind: PlannedTestKind,
    timeout_secs: Option<u64>,
    /// `test/parallel` is Node's own declaration that a file tolerates
    /// neighbours: those tests take a private temporary directory and refuse
    /// `common.PORT`. Everything else runs alone.
    concurrent: bool,
}

#[derive(Debug, Clone)]
enum PlannedTestKind {
    NodeJs(PathBuf),
    Command { program: String, args: Vec<String> },
}

const NODE_CHECKOUT: &str = "tests/node-compat/node";
const NODE_TEST_ROOT: &str = "tests/node-compat/node/test";
const NODE_SUITES: [&str; 2] = ["parallel", "sequential"];
const REPORT_DIR: &str = "tests/node-compat/reports";
const SITE_DATA: &str = "docs/site/public/node-conformance/data.json";
const WATCHDOG_MIN_GRACE_SECS: u64 = 5;

/// The most test process trees that may be alive at once, whatever `--jobs`
/// or the core count would otherwise allow.
const MAX_CONCURRENT_TESTS: usize = 16;

// Node's own test harness allows 120 seconds per test; several http
// lifecycle tests legitimately ride the server's 65-second keep-alive
// default, so a shorter watchdog misclassifies them as hangs.
fn default_timeout_secs() -> u64 {
    120
}

pub fn run(options: RunOptions) -> Result<RunReport> {
    let config = load_config(&options.config_path)?;
    ensure_node_tests_present(&options.workspace_root)?;
    let otter_bin = ensure_otter_binary(&options)?;
    let tests = collect_tests(&options.workspace_root, &config, &options)?;
    if tests.is_empty() {
        bail!("node-compat selected zero tests");
    }

    let started_at = Instant::now();
    let timeout_secs = options.timeout_secs.unwrap_or(config.timeout_secs);
    let results = run_tests(&options, &otter_bin, timeout_secs, &tests)?;

    let report = RunReport {
        timestamp: Utc::now(),
        otter_version: env!("CARGO_PKG_VERSION").to_string(),
        otter_binary: otter_bin.display().to_string(),
        node_commit: git_commit(&options.workspace_root.join(NODE_CHECKOUT)),
        engine_commit: git_commit(&options.workspace_root),
        duration_secs: started_at.elapsed().as_secs_f64(),
        summary: summarize_results(&results),
        results,
    };
    write_report(&options.workspace_root, &report)?;
    if options.is_full_corpus() {
        write_conformance_markdown(&options.workspace_root, &report)?;
        write_site_data(&options.workspace_root, &report)?;
    }
    Ok(report)
}

/// Execute the plan in two phases: everything that owns shared process state
/// (`test/sequential`, configured commands) runs alone first, then the
/// `test/parallel` files run across workers. Keeping the phases apart means a
/// concurrent child never competes with a test that assumes it owns the port,
/// the working directory, or the machine's timing.
fn run_tests(
    options: &RunOptions,
    otter_bin: &Path,
    timeout_secs: u64,
    tests: &[PlannedTest],
) -> Result<Vec<TestResult>> {
    let total = tests.len();
    let progress = AtomicUsize::new(0);
    let mut results: Vec<Option<TestResult>> = (0..total).map(|_| None).collect();

    for (index, test) in tests
        .iter()
        .enumerate()
        .filter(|(_, test)| !test.concurrent)
    {
        let result = run_one_test(
            &options.workspace_root,
            otter_bin,
            timeout_secs,
            test,
            index,
        )?;
        report_progress(&progress, total, &result);
        results[index] = Some(result);
    }

    let concurrent: Vec<(usize, &PlannedTest)> = tests
        .iter()
        .enumerate()
        .filter(|(_, test)| test.concurrent)
        .collect();
    if !concurrent.is_empty() {
        let workers = options.worker_count().min(concurrent.len());
        eprintln!(
            "node-compat: {} concurrent tests across {workers} workers",
            concurrent.len()
        );
        for (index, result) in run_concurrent_phase(
            options,
            otter_bin,
            timeout_secs,
            &concurrent,
            workers,
            &progress,
        )? {
            results[index] = Some(result);
        }
    }

    Ok(results
        .into_iter()
        .map(|result| result.expect("every planned test produces a result"))
        .collect())
}

/// Hand out the concurrent tests through a shared cursor so a slow file never
/// stalls a worker that could be running the next one. Results carry their
/// planned index back, which is what keeps the report in corpus order.
fn run_concurrent_phase(
    options: &RunOptions,
    otter_bin: &Path,
    timeout_secs: u64,
    tests: &[(usize, &PlannedTest)],
    workers: usize,
    progress: &AtomicUsize,
) -> Result<Vec<(usize, TestResult)>> {
    let total = progress.load(Ordering::Relaxed) + tests.len();
    let cursor = AtomicUsize::new(0);
    let collected: Mutex<Vec<(usize, TestResult)>> = Mutex::new(Vec::with_capacity(tests.len()));
    let failure: Mutex<Option<anyhow::Error>> = Mutex::new(None);

    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    if failure
                        .lock()
                        .expect("failure slot is not poisoned")
                        .is_some()
                    {
                        return;
                    }
                    let next = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some((index, test)) = tests.get(next) else {
                        return;
                    };
                    match run_one_test(
                        &options.workspace_root,
                        otter_bin,
                        timeout_secs,
                        test,
                        *index,
                    ) {
                        Ok(result) => {
                            report_progress(progress, total, &result);
                            collected
                                .lock()
                                .expect("result collector is not poisoned")
                                .push((*index, result));
                        }
                        Err(error) => {
                            *failure.lock().expect("failure slot is not poisoned") = Some(error);
                            return;
                        }
                    }
                }
            });
        }
    });

    if let Some(error) = failure.into_inner().expect("failure slot is not poisoned") {
        return Err(error);
    }
    Ok(collected
        .into_inner()
        .expect("result collector is not poisoned"))
}

/// Report a finished test as soon as it lands. Passing files print nothing but
/// the counter, so a long run stays readable and every failure is visible while
/// the run is still going.
fn report_progress(progress: &AtomicUsize, total: usize, result: &TestResult) {
    let done = progress.fetch_add(1, Ordering::Relaxed) + 1;
    let label = match result.outcome {
        Outcome::Pass => return eprintln!("[{done}/{total}] {}", result.path),
        Outcome::Fail => "FAIL",
        Outcome::Skipped => "SKIP",
        Outcome::Timeout => "TIMEOUT",
        Outcome::Crashed => "CRASH",
    };
    eprintln!(
        "[{done}/{total}] {label} {} ({} ms)",
        result.path, result.duration_ms
    );
}

/// Resolve the checked-out revision of a repository so every reported test can
/// link to the exact source it ran against. Missing git metadata is reported as
/// `unknown` rather than failing the run.
fn git_commit(repo: &Path) -> String {
    Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(repo)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|commit| commit.trim().to_string())
        .filter(|commit| !commit.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn load_config(path: &Path) -> Result<NodeCompatConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read node-compat config '{}'", path.display()))?;
    toml::from_str(&raw).context("failed to parse node-compat config")
}

fn ensure_node_tests_present(workspace_root: &Path) -> Result<()> {
    let root = workspace_root.join(NODE_TEST_ROOT);
    if root.exists() {
        return Ok(());
    }
    bail!(
        "official Node.js tests are missing at '{}'; run `bash scripts/fetch-node-tests.sh` first",
        root.display()
    )
}


/// The subset of a test's `// Flags:` line the engine understands. V8-specific
/// flags are skipped (the harness's own flag re-exec stays disabled), but the
/// Node-level switches Otter implements are honored, so tests that assert on
/// them run the way `node` runs them.
fn supported_test_flags(file_path: &Path) -> Vec<String> {
    const SUPPORTED_PREFIXES: &[&str] = &[
        "--title=",
        "--disable-warning=",
        "--max-http-header-size=",
        "--network-family-autoselection-attempt-timeout=",
        "--unhandled-rejections=",
        "--input-type=",
    ];
    const SUPPORTED_EXACT: &[&str] = &[
        "--expose-gc",
        "--expose_gc",
        "--expose-internals",
        "--expose_internals",
        "--experimental-stream-iter",
        "--network-family-autoselection",
        "--no-network-family-autoselection",
        "--no-warnings",
        "--no-deprecation",
        "--throw-deprecation",
        "--trace-warnings",
        "--pending-deprecation",
    ];
    let Ok(source) = std::fs::read_to_string(file_path) else {
        return Vec::new();
    };
    let mut flags = Vec::new();
    for line in source.lines().take(10) {
        let Some(rest) = line.trim().strip_prefix("// Flags:") else {
            continue;
        };
        for flag in rest.split_whitespace() {
            let supported = SUPPORTED_EXACT.contains(&flag)
                || SUPPORTED_PREFIXES
                    .iter()
                    .any(|prefix| flag.starts_with(prefix));
            if supported {
                flags.push(flag.to_string());
            }
        }
    }
    flags
}

fn ensure_otter_binary(options: &RunOptions) -> Result<PathBuf> {
    if let Some(path) = &options.otter_bin {
        // The test child runs from the Node checkout root, so a relative
        // binary path must be pinned to an absolute one first.
        return Ok(path.canonicalize().unwrap_or_else(|_| path.clone()));
    }

    let binary = options.workspace_root.join("target/release/otter");

    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("-p")
        .arg("otter-cli")
        .current_dir(&options.workspace_root)
        .status()
        .context("failed to spawn `cargo build --release -p otter-cli`")?;
    if !status.success() {
        bail!("`cargo build --release -p otter-cli` failed while preparing node-compat runner");
    }
    Ok(binary)
}

fn collect_tests(
    workspace_root: &Path,
    config: &NodeCompatConfig,
    options: &RunOptions,
) -> Result<Vec<PlannedTest>> {
    let selected: HashMap<&str, ()> = options
        .selected_modules
        .iter()
        .map(|m| (m.as_str(), ()))
        .collect();
    let mut planned = Vec::new();

    for suite in NODE_SUITES {
        let root = workspace_root.join(NODE_TEST_ROOT).join(suite);
        if !root.exists() {
            continue;
        }
        let files = std::fs::read_dir(&root)
            .with_context(|| format!("failed to read Node test suite '{}'", root.display()))?;
        for entry in files.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if !matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("js" | "mjs")
            ) {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let module = module_of(file_name);
            if !selected.is_empty() && !selected.contains_key(module.as_str()) {
                continue;
            }
            if let Some(filter) = &options.substring_filter
                && !file_name.contains(filter)
            {
                continue;
            }
            planned.push(PlannedTest {
                display_path: format!("{suite}/{file_name}"),
                timeout_secs: config.module_timeouts.get(&module).copied(),
                module,
                kind: PlannedTestKind::NodeJs(path.clone()),
                concurrent: suite == "parallel",
            });
        }
    }

    for integration in &config.integration_tests {
        if !selected.is_empty() && !selected.contains_key(integration.module.as_str()) {
            continue;
        }
        if let Some(filter) = &options.substring_filter
            && !integration.name.contains(filter)
        {
            continue;
        }
        planned.push(PlannedTest {
            display_path: format!("{}/{}", integration.module, integration.name),
            module: integration.module.clone(),
            kind: PlannedTestKind::Command {
                program: integration.program.clone(),
                args: integration.args.clone(),
            },
            timeout_secs: integration.timeout_secs,
            concurrent: false,
        });
    }

    planned.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    if let Some(limit) = options.limit {
        planned.truncate(limit);
    }
    Ok(planned)
}

/// Group a test into a module the way Node names its own files: the segment
/// after the `test-` prefix (`test-fs-read-stream.js` -> `fs`). Files that do
/// not follow the convention keep their stem, so nothing is silently merged.
fn module_of(file_name: &str) -> String {
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _extension)| stem);
    let body = stem.strip_prefix("test-").unwrap_or(stem);
    body.split_once('-')
        .map_or(body, |(module, _rest)| module)
        .to_string()
}

fn run_one_test(
    workspace_root: &Path,
    otter_bin: &Path,
    timeout_secs: u64,
    test: &PlannedTest,
    serial_id: usize,
) -> Result<TestResult> {
    let started = Instant::now();
    let test_timeout_secs = test.timeout_secs.unwrap_or(timeout_secs);
    let watchdog_timeout = Duration::from_secs(emergency_watchdog_timeout_secs(test_timeout_secs));
    let stdout_path = temp_output_path("stdout", serial_id);
    let stderr_path = temp_output_path("stderr", serial_id);
    let stdout_file = File::create(&stdout_path).with_context(|| {
        format!(
            "failed to create stdout capture for '{}'",
            test.display_path
        )
    })?;
    let stderr_file = File::create(&stderr_path).with_context(|| {
        format!(
            "failed to create stderr capture for '{}'",
            test.display_path
        )
    })?;
    let mut command = match &test.kind {
        PlannedTestKind::NodeJs(file_path) => {
            let mut command = Command::new(otter_bin);
            command
                .arg(format!("--timeout={test_timeout_secs}"))
                .arg("--allow-all")
                .args(supported_test_flags(file_path))
                .arg(file_path)
                // The harness's `common` re-execs the test with V8-specific
                // `// Flags:`. Otter is a different engine, so disable that
                // flag-reexec path.
                .env("NODE_SKIP_FLAG_CHECK", "1")
                // `common/tmpdir.js` names its scratch directory
                // `.tmp.${TEST_SERIAL_ID}`, so a unique id per test is what
                // stops concurrent `tmpdir.refresh()` calls from wiping each
                // other's files.
                .env("TEST_SERIAL_ID", serial_id.to_string())
                // Color-control variables are test inputs in util/TTY.
                .env_remove("NO_COLOR")
                .env_remove("NODE_DISABLE_COLORS")
                .env_remove("FORCE_COLOR");
            if test.concurrent {
                // `common.PORT` is a fixed shared port; Node's harness throws on
                // it under this variable rather than letting two concurrent
                // tests bind it. A test that trips this belongs in
                // `test/sequential` upstream, and the thrown error names it.
                command.env("TEST_PARALLEL", "1");
            }
            command
        }
        PlannedTestKind::Command { program, args } => {
            let mut command = Command::new(program);
            command.args(args);
            command
        }
    };
    command
        // Node's own test runner executes from the checkout root; tests that
        // touch `process.cwd()` (chdir into `lib/`, resolve `./.env`) assume
        // exactly that directory.
        .current_dir(
            workspace_root
                .join(NODE_CHECKOUT)
                .canonicalize()
                .unwrap_or_else(|_| workspace_root.join(NODE_CHECKOUT)),
        )
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    configure_watchdog_process_group(&mut command);

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run otter for '{}'", test.display_path))?;
    // The test leads its own process group, so everything it starts is a
    // member and can be ended with it.
    let group = child.id();

    let timed_out = wait_for_child_with_watchdog(&mut child, watchdog_timeout)
        .with_context(|| format!("failed while waiting for '{}'", test.display_path))?;
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for '{}'", test.display_path))?;
    // A test that exits having left children behind must not leave them
    // running: a corpus of a few hundred such tests otherwise accumulates
    // processes until the machine is unusable. This runs on every path, not
    // only the timeout one.
    end_process_group(group);
    let duration_ms = started.elapsed().as_millis();

    let stderr = read_output_file(&stderr_path);
    let stdout = read_output_file(&stdout_path);
    let _ = std::fs::remove_file(&stdout_path);
    let _ = std::fs::remove_file(&stderr_path);
    let error_text = if stderr.is_empty() {
        stdout.clone()
    } else if stdout.is_empty() {
        stderr.clone()
    } else {
        format!("{stderr}\n{stdout}")
    };

    let outcome = if timed_out {
        Outcome::Timeout
    } else if status.success() {
        Outcome::Pass
    } else if is_timeout_error(&error_text) {
        Outcome::Timeout
    } else if status.code().is_some() {
        Outcome::Fail
    } else {
        Outcome::Crashed
    };

    Ok(TestResult {
        path: test.display_path.clone(),
        module: test.module.clone(),
        outcome,
        duration_ms,
        error: (!error_text.is_empty()).then_some(error_text),
    })
}

fn emergency_watchdog_timeout_secs(timeout_secs: u64) -> u64 {
    timeout_secs
        .saturating_mul(2)
        .max(timeout_secs.saturating_add(WATCHDOG_MIN_GRACE_SECS))
}

/// Capture files are named after the test's plan index as well as the clock:
/// two workers can start inside the same nanosecond, and a shared capture file
/// would attribute one child's output to another.
fn temp_output_path(stream: &str, serial_id: usize) -> PathBuf {
    let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    std::env::temp_dir().join(format!(
        "otter-node-compat-{}-{}-{}-{}.log",
        std::process::id(),
        serial_id,
        now,
        stream
    ))
}

fn read_output_file(path: &Path) -> String {
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .unwrap_or_default()
}

fn wait_for_child_with_watchdog(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<bool> {
    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .context("failed to poll child process status")?
            .is_some()
        {
            return Ok(false);
        }

        if started.elapsed() >= timeout {
            kill_with_watchdog(child);
            return Ok(true);
        }

        thread::sleep(Duration::from_millis(25));
    }
}

fn configure_watchdog_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        command.process_group(0);
    }
}

fn kill_with_watchdog(child: &mut std::process::Child) {
    end_process_group(child.id());
    let _ = child.kill();
}

/// End every process still in a test's group.
///
/// A group id stays reserved while any process is still a member, so the group
/// this names is either the test's own or already empty — in which case the
/// signal finds nothing and does nothing.
fn end_process_group(group: u32) {
    #[cfg(unix)]
    {
        if let Ok(group) = i32::try_from(group)
            && group > 0
        {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = group;
    }
}

fn is_timeout_error(message: &str) -> bool {
    message.contains("Interrupted")
        || message.contains("timed out")
        || message.contains("timeout after")
        || message.contains("execution timed out")
}

fn summarize_results(results: &[TestResult]) -> RunSummary {
    let mut summary = RunSummary {
        total: results.len(),
        passed: 0,
        failed: 0,
        skipped: 0,
        timeout: 0,
        crashed: 0,
        pass_rate: 0.0,
        by_module: BTreeMap::new(),
        failures: Vec::new(),
    };

    for result in results {
        let module_summary = summary.by_module.entry(result.module.clone()).or_default();
        module_summary.total += 1;

        match result.outcome {
            Outcome::Pass => {
                summary.passed += 1;
                module_summary.passed += 1;
            }
            Outcome::Fail => {
                summary.failed += 1;
                module_summary.failed += 1;
            }
            Outcome::Skipped => {
                summary.skipped += 1;
                module_summary.skipped += 1;
            }
            Outcome::Timeout => {
                summary.timeout += 1;
                module_summary.timeout += 1;
            }
            Outcome::Crashed => {
                summary.crashed += 1;
                module_summary.crashed += 1;
            }
        }

        if matches!(
            result.outcome,
            Outcome::Fail | Outcome::Timeout | Outcome::Crashed
        ) && let Some(error) = &result.error
        {
            summary.failures.push(FailureSummary {
                path: result.path.clone(),
                module: result.module.clone(),
                error: error.clone(),
            });
        }
    }

    if summary.total > 0 {
        summary.pass_rate = (summary.passed as f64 / summary.total as f64) * 100.0;
    }

    summary
}

fn write_report(workspace_root: &Path, report: &RunReport) -> Result<()> {
    let report_dir = workspace_root.join(REPORT_DIR);
    std::fs::create_dir_all(&report_dir)
        .with_context(|| format!("failed to create report dir '{}'", report_dir.display()))?;

    let pretty = serde_json::to_string_pretty(report).context("failed to serialize report")?;
    let file_name = format!("run_{}.json", report.timestamp.format("%Y%m%d_%H%M%S"));
    std::fs::write(report_dir.join(file_name), &pretty).context("failed to write report file")?;
    std::fs::write(report_dir.join("latest.json"), &pretty)
        .context("failed to update latest node-compat report")?;
    Ok(())
}

/// Regenerate `NODE_CONFORMANCE.md` from a run report. The document is always
/// machine-generated from the latest run — never hand-edited — mirroring the
/// Test262 conformance workflow.
fn write_conformance_markdown(workspace_root: &Path, report: &RunReport) -> Result<()> {
    let s = &report.summary;
    let mut out = String::new();
    out.push_str("# Node.js Conformance\n\n");
    out.push_str(
        "> Generated by `cargo run -p otter-node-compat`. Do not edit by hand — \
         rerun the conformance suite to refresh.\n\n",
    );
    out.push_str(&format!(
        "- Generated: `{}`\n",
        report.timestamp.format("%Y-%m-%d %H:%M:%SZ")
    ));
    out.push_str(&format!("- Otter version: `{}`\n", report.otter_version));
    out.push_str(&format!("- Otter binary: `{}`\n", report.otter_binary));
    out.push_str(&format!("- Otter commit: `{}`\n", report.engine_commit));
    out.push_str(&format!(
        "- Node corpus: [`nodejs/node@{}`](https://github.com/nodejs/node/tree/{}/test)\n",
        short_commit(&report.node_commit),
        report.node_commit
    ));
    out.push_str(&format!("- Duration: {:.1}s\n\n", report.duration_secs));

    out.push_str("## Summary\n\n");
    out.push_str(&format!(
        "**{}/{} passed ({:.1}%)**\n\n",
        s.passed, s.total, s.pass_rate
    ));
    out.push_str("| Outcome | Count |\n|---|---|\n");
    out.push_str(&format!("| pass | {} |\n", s.passed));
    out.push_str(&format!("| fail | {} |\n", s.failed));
    out.push_str(&format!("| timeout | {} |\n", s.timeout));
    out.push_str(&format!("| crashed | {} |\n", s.crashed));
    out.push_str(&format!("| skipped | {} |\n\n", s.skipped));

    out.push_str("## By module\n\n");
    out.push_str(
        "| Module | Pass | Fail | Timeout | Crashed | Total | Pass rate |\n\
         |---|---|---|---|---|---|---|\n",
    );
    for (module, m) in &s.by_module {
        let rate = if m.total > 0 {
            (m.passed as f64 / m.total as f64) * 100.0
        } else {
            0.0
        };
        out.push_str(&format!(
            "| {module} | {} | {} | {} | {} | {} | {rate:.1}% |\n",
            m.passed, m.failed, m.timeout, m.crashed, m.total
        ));
    }
    out.push('\n');

    let timeouts: Vec<_> = report
        .results
        .iter()
        .filter(|result| result.outcome == Outcome::Timeout)
        .collect();
    if !timeouts.is_empty() {
        out.push_str("## Timeouts\n\n");
        for result in timeouts {
            out.push_str(&format!(
                "- `{}` ({:.1}s)\n",
                result.path,
                result.duration_ms as f64 / 1_000.0
            ));
        }
        out.push('\n');
    }

    let crashes: Vec<_> = report
        .results
        .iter()
        .filter(|result| result.outcome == Outcome::Crashed)
        .collect();
    if !crashes.is_empty() {
        out.push_str("## Crashes\n\n");
        for result in crashes {
            out.push_str(&format!("- `{}`\n", result.path));
        }
        out.push('\n');
    }

    out.push_str(
        "The interactive per-test breakdown, including every failure reason, \
         lives on the documentation site under Conformance → Node.js.\n",
    );

    std::fs::write(workspace_root.join("NODE_CONFORMANCE.md"), out)
        .context("failed to write NODE_CONFORMANCE.md")?;
    Ok(())
}

fn short_commit(commit: &str) -> &str {
    if commit.len() >= 8 {
        &commit[..8]
    } else {
        commit
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct SectionTotals {
    total: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    crashed: usize,
    timed_out: usize,
    oom: usize,
}

#[derive(Debug, Clone, Serialize)]
struct FailingTest {
    path: String,
    outcome: &'static str,
    reason: String,
}

#[derive(Debug, Clone, Serialize)]
struct SiteData {
    node_commit: String,
    engine_commit: String,
    ran_at: DateTime<Utc>,
    totals: SectionTotals,
    by_section: BTreeMap<String, SectionTotals>,
    failing_tests: Vec<FailingTest>,
}

/// Publish the dashboard payload consumed by the documentation site. The shape
/// mirrors the Test262 baseline so both suites render through one component.
fn write_site_data(workspace_root: &Path, report: &RunReport) -> Result<()> {
    let mut data = SiteData {
        node_commit: report.node_commit.clone(),
        engine_commit: report.engine_commit.clone(),
        ran_at: report.timestamp,
        totals: SectionTotals::default(),
        by_section: BTreeMap::new(),
        failing_tests: Vec::new(),
    };

    for result in &report.results {
        let suite = result
            .path
            .split_once('/')
            .map_or("integration", |(suite, _file)| suite);
        let section = format!("{suite}/{}", result.module);
        let entry = data.by_section.entry(section).or_default();
        for totals in [&mut data.totals, entry] {
            totals.total += 1;
            match result.outcome {
                Outcome::Pass => totals.passed += 1,
                Outcome::Fail => totals.failed += 1,
                Outcome::Skipped => totals.skipped += 1,
                Outcome::Timeout => totals.timed_out += 1,
                Outcome::Crashed => totals.crashed += 1,
            }
        }

        // Outcome names are the dashboard's badge classes, shared with the
        // Test262 baseline.
        let outcome = match result.outcome {
            Outcome::Fail => "fail",
            Outcome::Timeout => "timeout",
            Outcome::Crashed => "crash",
            Outcome::Pass | Outcome::Skipped => continue,
        };
        data.failing_tests.push(FailingTest {
            path: result.path.clone(),
            outcome,
            reason: failure_reason(result),
        });
    }

    let path = workspace_root.join(SITE_DATA);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create site data dir '{}'", parent.display()))?;
    }
    let json = serde_json::to_string(&data).context("failed to serialize site conformance data")?;
    std::fs::write(&path, json)
        .with_context(|| format!("failed to write site data '{}'", path.display()))?;
    Ok(())
}

/// Condense a captured child's output into one dashboard-sized line. Node tests
/// print full assertion dumps, so the first meaningful line carries the signal.
fn failure_reason(result: &TestResult) -> String {
    const MAX_REASON_CHARS: usize = 240;
    let Some(error) = &result.error else {
        return match result.outcome {
            Outcome::Timeout => "timed out with no output".to_string(),
            Outcome::Crashed => "crashed with no output".to_string(),
            _ => "no output".to_string(),
        };
    };
    let line = error
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output");
    if line.chars().count() <= MAX_REASON_CHARS {
        return line.to_string();
    }
    let truncated: String = line.chars().take(MAX_REASON_CHARS).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use super::{Outcome, RunOptions, module_of, run};

    use crate::MAX_CONCURRENT_TESTS;
    use std::path::PathBuf;

    /// The worker count is a ceiling on live process trees, not a suggestion:
    /// one worker is a whole tree, so a width taken from `--jobs` or from the
    /// core count unclamped is what makes a corpus run swamp a machine.
    #[test]
    fn worker_count_never_exceeds_the_ceiling() {
        let workspace = PathBuf::from("/nonexistent");

        let mut options = RunOptions::new(workspace.clone());
        options.jobs = Some(1024);
        assert_eq!(options.worker_count(), MAX_CONCURRENT_TESTS);

        options.jobs = Some(MAX_CONCURRENT_TESTS + 1);
        assert_eq!(options.worker_count(), MAX_CONCURRENT_TESTS);

        // Below the ceiling `--jobs` still says exactly what it says, and zero
        // is not a width a run can have.
        options.jobs = Some(3);
        assert_eq!(options.worker_count(), 3);
        options.jobs = Some(0);
        assert_eq!(options.worker_count(), 1);

        // The default is derived from the cores and is capped the same way.
        options.jobs = None;
        assert!(options.worker_count() <= MAX_CONCURRENT_TESTS);
        assert!(options.worker_count() >= 1);
    }

    #[test]
    fn modules_follow_node_test_naming() {
        assert_eq!(module_of("test-util-format.js"), "util");
        assert_eq!(module_of("test-path.js"), "path");
        assert_eq!(module_of("test-fs-read-stream.mjs"), "fs");
        assert_eq!(module_of("worker-metadata.js"), "worker");
    }

    #[test]
    fn cli_timeout_diagnostic_is_classified_as_timeout() {
        assert!(super::is_timeout_error("error: timeout after 10000 ms"));
    }

    fn temp_test_root(name: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        dir.push(format!("otter-node-compat-{name}-{unique}"));
        std::fs::create_dir_all(dir.join("tests/node-compat/node/test/parallel"))
            .expect("node-compat test root should exist");
        dir
    }

    #[test]
    fn external_watchdog_marks_hanging_test_as_timeout() {
        let workspace = temp_test_root("timeout");
        std::fs::write(
            workspace.join("node_compat_config.toml"),
            "timeout_secs = 1\n",
        )
        .expect("config should write");
        std::fs::write(
            workspace.join("tests/node-compat/node/test/parallel/test-process-hang.js"),
            "// synthetic hang test\n",
        )
        .expect("test file should write");

        let fake_otter = workspace.join("fake-otter.sh");
        std::fs::write(&fake_otter, "#!/bin/sh\nexec sleep 30\n").expect("fake otter should write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&fake_otter)
                .expect("fake otter metadata should exist")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&fake_otter, permissions)
                .expect("fake otter should be executable");
        }

        let mut options = RunOptions::new(workspace.clone());
        options.selected_modules = vec!["process".to_string()];
        options.timeout_secs = Some(1);
        options.otter_bin = Some(fake_otter);

        let started = Instant::now();
        let report = run(options).expect("node-compat run should complete");

        assert_eq!(report.summary.total, 1);
        assert_eq!(report.summary.timeout, 1);
        assert_eq!(report.results[0].outcome, Outcome::Timeout);
        assert!(started.elapsed().as_secs() < 10);
        assert!(
            !workspace.join("NODE_CONFORMANCE.md").exists(),
            "a module-scoped run must not publish a conformance baseline"
        );
    }

    #[test]
    fn concurrent_phase_keeps_corpus_order_and_isolates_children() {
        let workspace = temp_test_root("concurrent");
        std::fs::create_dir_all(workspace.join("tests/node-compat/node/test/sequential"))
            .expect("sequential suite should exist");
        std::fs::write(
            workspace.join("node_compat_config.toml"),
            "timeout_secs = 5\n",
        )
        .expect("config should write");
        for name in ["test-fs-a.js", "test-fs-b.js", "test-fs-c.js"] {
            std::fs::write(
                workspace
                    .join("tests/node-compat/node/test/parallel")
                    .join(name),
                "// synthetic test\n",
            )
            .expect("parallel test should write");
        }
        std::fs::write(
            workspace.join("tests/node-compat/node/test/sequential/test-fs-port.js"),
            "// synthetic test\n",
        )
        .expect("sequential test should write");

        // Reports the isolation variables the child actually received, so the
        // assertions below check the environment rather than the plan.
        let fake_otter = workspace.join("fake-otter.sh");
        std::fs::write(
            &fake_otter,
            "#!/bin/sh\necho \"serial=$TEST_SERIAL_ID parallel=${TEST_PARALLEL:-0}\" >&2\nexit 1\n",
        )
        .expect("fake otter should write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&fake_otter)
                .expect("fake otter metadata should exist")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&fake_otter, permissions)
                .expect("fake otter should be executable");
        }

        let mut options = RunOptions::new(workspace.clone());
        options.otter_bin = Some(fake_otter);
        options.jobs = Some(3);
        let report = run(options).expect("node-compat run should complete");

        let paths: Vec<&str> = report
            .results
            .iter()
            .map(|result| result.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "parallel/test-fs-a.js",
                "parallel/test-fs-b.js",
                "parallel/test-fs-c.js",
                "sequential/test-fs-port.js",
            ],
            "results must follow corpus order, not completion order"
        );

        let mut serial_ids = Vec::new();
        for result in &report.results {
            let error = result.error.as_deref().expect("child reported its env");
            let expected_parallel = if result.path.starts_with("parallel/") {
                "parallel=1"
            } else {
                "parallel=0"
            };
            assert!(
                error.contains(expected_parallel),
                "{} ran with the wrong parallel marker: {error}",
                result.path
            );
            serial_ids.push(error.to_string());
        }
        serial_ids.sort();
        serial_ids.dedup();
        assert_eq!(
            serial_ids.len(),
            report.results.len(),
            "every child needs its own TEST_SERIAL_ID"
        );
    }

    #[test]
    fn full_corpus_run_publishes_baseline_and_dashboard_data() {
        let workspace = temp_test_root("baseline");
        std::fs::write(
            workspace.join("node_compat_config.toml"),
            "timeout_secs = 1\n",
        )
        .expect("config should write");
        std::fs::write(
            workspace.join("tests/node-compat/node/test/parallel/test-fs-open.js"),
            "// synthetic failing test\n",
        )
        .expect("test file should write");

        let fake_otter = workspace.join("fake-otter.sh");
        std::fs::write(&fake_otter, "#!/bin/sh\necho boom >&2\nexit 1\n")
            .expect("fake otter should write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&fake_otter)
                .expect("fake otter metadata should exist")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&fake_otter, permissions)
                .expect("fake otter should be executable");
        }

        let mut options = RunOptions::new(workspace.clone());
        options.otter_bin = Some(fake_otter);
        let report = run(options).expect("node-compat run should complete");

        assert_eq!(report.summary.failed, 1);
        assert!(workspace.join("NODE_CONFORMANCE.md").exists());

        let data = std::fs::read_to_string(workspace.join(super::SITE_DATA))
            .expect("dashboard data should be published");
        let data: serde_json::Value =
            serde_json::from_str(&data).expect("dashboard data should be JSON");
        assert_eq!(data["totals"]["failed"], 1);
        assert_eq!(data["by_section"]["parallel/fs"]["total"], 1);
        assert_eq!(data["failing_tests"][0]["path"], "parallel/test-fs-open.js");
        assert_eq!(data["failing_tests"][0]["outcome"], "fail");
        assert_eq!(data["failing_tests"][0]["reason"], "boom");
    }
}
