use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use glob::Pattern;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct NodeCompatConfig {
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    pub modules: BTreeMap<String, NodeCompatModuleConfig>,
}

#[derive(Debug, Deserialize)]
pub struct NodeCompatModuleConfig {
    pub patterns: Vec<String>,
    #[serde(default)]
    pub skip: Vec<String>,
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
    file_path: PathBuf,
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
        .arg("otterjs")
        .current_dir(&options.workspace_root)
        .status()
        .context("failed to spawn `cargo build -p otterjs`")?;
    if !status.success() {
        bail!("`cargo build -p otterjs` failed while preparing node-compat runner");
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

    let requested_modules: Vec<_> = if options.selected_modules.is_empty() {
        config.modules.keys().cloned().collect()
    } else {
        options.selected_modules.clone()
    };

    let mut planned = Vec::new();
    for module in requested_modules {
        let module_config = config
            .modules
            .get(&module)
            .ok_or_else(|| anyhow!("node-compat module '{module}' is not present in config"))?;
        let patterns = module_config
            .patterns
            .iter()
            .map(|pattern| Pattern::new(pattern))
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("invalid glob pattern in module '{module}'"))?;
        let skipped: HashMap<_, _> = module_config
            .skip
            .iter()
            .map(|item| (item.as_str(), ()))
            .collect();

        for path in &available {
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if skipped.contains_key(file_name) {
                continue;
            }
            if !patterns.iter().any(|pattern| pattern.matches(file_name)) {
                continue;
            }
            if let Some(filter) = &options.substring_filter
                && !file_name.contains(filter)
            {
                continue;
            }

            planned.push(PlannedTest {
                module: module.clone(),
                display_path: format!("{module}/{file_name}"),
                file_path: path.clone(),
            });
        }
    }

    planned.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    if let Some(limit) = options.limit {
        planned.truncate(limit);
    }
    Ok(planned)
}

fn run_one_test(
    workspace_root: &Path,
    otter_bin: &Path,
    timeout_secs: u64,
    test: &PlannedTest,
) -> Result<TestResult> {
    let started = Instant::now();
    let watchdog_timeout = Duration::from_secs(emergency_watchdog_timeout_secs(timeout_secs));
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
    let mut command = Command::new(otter_bin);
    command
        .arg("--allow-all")
        .arg("--timeout")
        .arg(timeout_secs.to_string())
        .arg(&test.file_path)
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

#[cfg(test)]
mod tests {
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use super::{Outcome, RunOptions, run};

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
