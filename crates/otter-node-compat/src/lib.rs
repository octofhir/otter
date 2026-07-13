//! Config-driven Node.js compatibility runner.
//!
//! # Contents
//! - TOML selection of official JavaScript tests by module and filename glob.
//! - Configured integration commands for compatibility surfaces that need
//!   build fixtures, such as native Node-API addons.
//! - Watchdog execution and generated JSON/Markdown conformance reports.
//!
//! # Invariants
//! - Only tests selected by `node_compat_config.toml` enter the report.
//! - Every child runs in its own process group and is terminated by the
//!   external watchdog on timeout.
//! - Configured commands use owned strings and never cross VM/GC boundaries.
//!
//! # See also
//! - `node_compat_config.toml`
//! - `NODE_CONFORMANCE.md`

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct NodeCompatConfig {
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    pub modules: BTreeMap<String, NodeCompatModuleConfig>,
    #[serde(default)]
    pub integration_tests: Vec<NodeCompatIntegrationTest>,
}

#[derive(Debug, Deserialize)]
pub struct NodeCompatModuleConfig {
    pub patterns: Vec<String>,
    #[serde(default)]
    pub skip: Vec<String>,
    pub timeout_secs: Option<u64>,
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
}

impl RunOptions {
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
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub timestamp: DateTime<Utc>,
    pub otter_version: String,
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
}

#[derive(Debug, Clone)]
enum PlannedTestKind {
    NodeJs(PathBuf),
    Command { program: String, args: Vec<String> },
}

const NODE_TEST_ROOT: &str = "tests/node-compat/node/test/parallel";
const REPORT_DIR: &str = "tests/node-compat/reports";
const WATCHDOG_MIN_GRACE_SECS: u64 = 5;

fn default_timeout_secs() -> u64 {
    10
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
    let mut results = Vec::with_capacity(tests.len());
    let timeout_secs = options.timeout_secs.unwrap_or(config.timeout_secs);

    for test in tests {
        results.push(run_one_test(
            &options.workspace_root,
            &otter_bin,
            timeout_secs,
            &test,
        )?);
    }

    let report = RunReport {
        timestamp: Utc::now(),
        otter_version: env!("CARGO_PKG_VERSION").to_string(),
        duration_secs: started_at.elapsed().as_secs_f64(),
        summary: summarize_results(&results),
        results,
    };
    write_report(&options.workspace_root, &report)?;
    write_conformance_markdown(&options.workspace_root, &report)?;
    Ok(report)
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

fn ensure_otter_binary(options: &RunOptions) -> Result<PathBuf> {
    if let Some(path) = &options.otter_bin {
        return Ok(path.clone());
    }

    let binary = options.workspace_root.join("target/debug/otter");
    if binary.exists() {
        return Ok(binary);
    }

    let status = Command::new("cargo")
        .arg("build")
        .arg("-p")
        .arg("otter-cli")
        .current_dir(&options.workspace_root)
        .status()
        .context("failed to spawn `cargo build -p otter-cli`")?;
    if !status.success() {
        bail!("`cargo build -p otter-cli` failed while preparing node-compat runner");
    }
    Ok(binary)
}

fn collect_tests(
    workspace_root: &Path,
    config: &NodeCompatConfig,
    options: &RunOptions,
) -> Result<Vec<PlannedTest>> {
    let root = workspace_root.join(NODE_TEST_ROOT);
    let files = std::fs::read_dir(&root)
        .with_context(|| format!("failed to read Node test root '{}'", root.display()))?;

    let available: Vec<_> = files
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("js"))
        .collect();

    let selected: HashMap<&str, ()> = options
        .selected_modules
        .iter()
        .map(|m| (m.as_str(), ()))
        .collect();
    let mut planned = Vec::new();
    for (module, module_config) in &config.modules {
        if !selected.is_empty() && !selected.contains_key(module.as_str()) {
            continue;
        }
        for path in &available {
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if module_config.skip.iter().any(|skip| skip == file_name)
                || !module_config
                    .patterns
                    .iter()
                    .any(|pattern| wildcard_matches(pattern, file_name))
            {
                continue;
            }
            if let Some(filter) = &options.substring_filter
                && !file_name.contains(filter)
            {
                continue;
            }
            planned.push(PlannedTest {
                display_path: format!("{module}/{file_name}"),
                module: module.clone(),
                kind: PlannedTestKind::NodeJs(path.clone()),
                timeout_secs: module_config.timeout_secs,
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
        });
    }

    planned.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    if let Some(limit) = options.limit {
        planned.truncate(limit);
    }
    Ok(planned)
}

/// Match the config's filename globs without parsing or transforming JS.
/// Node test names are ASCII; `*` and `?` use the conventional glob meaning.
fn wildcard_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let (mut pattern_index, mut text_index) = (0, 0);
    let (mut star, mut star_text_index) = (None, 0);

    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == text[text_index])
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_text_index = text_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_text_index += 1;
            text_index = star_text_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn run_one_test(
    workspace_root: &Path,
    otter_bin: &Path,
    timeout_secs: u64,
    test: &PlannedTest,
) -> Result<TestResult> {
    let started = Instant::now();
    let test_timeout_secs = test.timeout_secs.unwrap_or(timeout_secs);
    let watchdog_timeout = Duration::from_secs(emergency_watchdog_timeout_secs(test_timeout_secs));
    let stdout_path = temp_output_path("stdout");
    let stderr_path = temp_output_path("stderr");
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
                .arg(file_path)
                // The harness's `common` re-execs the test with V8-specific
                // `// Flags:`. Otter is a different engine, so disable that
                // flag-reexec path.
                .env("NODE_SKIP_FLAG_CHECK", "1")
                // Color-control variables are test inputs in util/TTY.
                .env_remove("NO_COLOR")
                .env_remove("NODE_DISABLE_COLORS")
                .env_remove("FORCE_COLOR");
            command
        }
        PlannedTestKind::Command { program, args } => {
            let mut command = Command::new(program);
            command.args(args);
            command
        }
    };
    command
        .current_dir(workspace_root)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    configure_watchdog_process_group(&mut command);

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run otter for '{}'", test.display_path))?;

    let timed_out = wait_for_child_with_watchdog(&mut child, watchdog_timeout)
        .with_context(|| format!("failed while waiting for '{}'", test.display_path))?;
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for '{}'", test.display_path))?;
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

fn temp_output_path(stream: &str) -> PathBuf {
    let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    std::env::temp_dir().join(format!(
        "otter-node-compat-{}-{}-{}.log",
        std::process::id(),
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
    #[cfg(unix)]
    {
        let pid = child.id();
        if pid > 0 {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }

    let _ = child.kill();
}

fn is_timeout_error(message: &str) -> bool {
    message.contains("Interrupted")
        || message.contains("timed out")
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
            }
            Outcome::Crashed => {
                summary.crashed += 1;
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
    out.push_str("| Module | Pass | Total | Pass rate |\n|---|---|---|---|\n");
    for (module, m) in &s.by_module {
        let rate = if m.total > 0 {
            (m.passed as f64 / m.total as f64) * 100.0
        } else {
            0.0
        };
        out.push_str(&format!(
            "| {module} | {} | {} | {rate:.1}% |\n",
            m.passed, m.total
        ));
    }
    out.push('\n');

    std::fs::write(workspace_root.join("NODE_CONFORMANCE.md"), out)
        .context("failed to write NODE_CONFORMANCE.md")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use super::{Outcome, RunOptions, run, wildcard_matches};

    #[test]
    fn config_filename_globs_match_expected_node_tests() {
        assert!(wildcard_matches("test-util-*.js", "test-util-format.js"));
        assert!(wildcard_matches("test-path.js", "test-path.js"));
        assert!(!wildcard_matches("test-path.js", "test-path-extra.js"));
        assert!(wildcard_matches("test-?-x.js", "test-a-x.js"));
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
            "timeout_secs = 1\n\n[modules.process]\npatterns = [\"test-process-hang.js\"]\nskip = []\n",
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
    }
}
