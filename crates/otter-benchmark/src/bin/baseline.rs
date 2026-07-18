//! Reproducible capture and publication of the current engine baseline.
//!
//! # Contents
//! - `capture` runs the fixed engine-only matrix serially into an ignored
//!   evidence directory.
//! - `publish` revalidates one complete capture before materializing the one
//!   tracked current baseline.
//!
//! # Invariants
//! - Every measured child observes the same clean release commit.
//! - The outer watchdog is capture metadata only; child benchmark records keep
//!   `sampling.timeoutMs` null because the engine process did not enforce it.
//! - Raw output and non-scoreable observations remain in the ignored capture.
//! - Publication has no compatibility reader or schema/version field.
//! - No Node, Web, package installation, profiler, or environment-selected JIT
//!   policy participates in the matrix.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use otter_benchmark::{
    BenchmarkConfiguration, BenchmarkIdentity, BenchmarkResult, CacheState, ExecutionSurface,
    GcPolicy, JitPolicy, MetricRole, MetricValue, RuntimeReuse, SamplingPlan,
    current_build_profile,
};
use serde::{Deserialize, Serialize};

const DEFAULT_OUTER_TIMEOUT_MS: u64 = 120_000;
const CALL_ITERATIONS: u32 = 100_000;
const CALL_SAMPLES: u32 = 20;
const CALL_WARMUPS: u32 = 3;
const COMPILE_SAMPLES: u32 = 100;
const COMPILE_WARMUPS: u32 = 10;
const MEMORY_ITERATIONS: u32 = 1_000_000;
const MEMORY_SAMPLES: u32 = 5;
const MODULE_SAMPLES: u32 = 20;
const MODULE_WARMUPS: u32 = 5;

#[derive(Debug, Clone, Copy)]
struct KernelSpec {
    slug: &'static str,
    source: &'static str,
    expected: &'static str,
    warmups: u32,
    samples: u32,
}

const KERNELS: [KernelSpec; 4] = [
    KernelSpec {
        slug: "method-call-monomorphic",
        source: "benchmarks/scripts/method-call-monomorphic.js",
        expected: "500003500000",
        warmups: 8,
        samples: 15,
    },
    KernelSpec {
        slug: "branch-phi",
        source: "benchmarks/scripts/branch-phi.js",
        expected: "-6000000",
        warmups: 5,
        samples: 20,
    },
    KernelSpec {
        slug: "boxed-double-property",
        source: "benchmarks/scripts/boxed-double-property.js",
        expected: "4000000",
        warmups: 5,
        samples: 15,
    },
    KernelSpec {
        slug: "dense-array",
        source: "benchmarks/scripts/dense-array.js",
        expected: "5234688",
        warmups: 5,
        samples: 15,
    },
];

#[derive(Debug, Parser)]
#[command(about = "Capture or publish the current Otter engine baseline")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build and run the fixed matrix into an ignored evidence directory.
    Capture {
        #[arg(long, default_value = "benchmarks/results")]
        output_root: PathBuf,
        #[arg(long, default_value_t = DEFAULT_OUTER_TIMEOUT_MS)]
        outer_timeout_ms: u64,
    },
    /// Revalidate a complete capture and create the one tracked baseline.
    Publish {
        #[arg(long)]
        capture: PathBuf,
    },
}

#[derive(Debug, Clone, Copy)]
struct Tier {
    cli: &'static str,
    policy: JitPolicy,
}

const TIERS: [Tier; 3] = [
    Tier {
        cli: "interpreter",
        policy: JitPolicy::Interpreter,
    },
    Tier {
        cli: "template",
        policy: JitPolicy::Template,
    },
    Tier {
        cli: "production-tiered",
        policy: JitPolicy::ProductionTiered,
    },
];

#[derive(Debug, Clone)]
struct BaselineCase {
    id: String,
    args: Vec<String>,
    benchmark: BenchmarkIdentity,
    configuration: BenchmarkConfiguration,
    sampling: SamplingPlan,
}

impl BaselineCase {
    fn argv(&self, engine_argv0: &str) -> Vec<String> {
        std::iter::once(engine_argv0.to_owned())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum CaptureStatus {
    Completed,
    OuterTimeout,
    SpawnFailed,
    InvalidRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CaptureRun {
    id: String,
    argv: Vec<String>,
    status: CaptureStatus,
    process_exit_code: Option<i32>,
    record_path: Option<String>,
    stdout_path: String,
    stderr_path: String,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CaptureManifest {
    commit: String,
    captured_at_unix_ms: u64,
    outer_timeout_ms: u64,
    engine_argv0: String,
    complete: bool,
    postflight_error: Option<String>,
    runs: Vec<CaptureRun>,
}

#[derive(Debug)]
struct ChildRun {
    status: ExitStatus,
    timed_out: bool,
}

fn parameters<const N: usize>(entries: [(&str, String); N]) -> BTreeMap<String, String> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn configuration(
    surface: ExecutionSurface,
    jit_policy: JitPolicy,
    gc_policy: GcPolicy,
    runtime_reuse: RuntimeReuse,
) -> BenchmarkConfiguration {
    BenchmarkConfiguration {
        surface,
        jit_policy,
        jit_osr_threshold: None,
        gc_policy,
        gc_stress_stride: None,
        runtime_reuse,
        cache_state: CacheState::NotApplicable,
    }
}

fn sampling(warmups: u32, samples: u32, iterations: u64) -> SamplingPlan {
    SamplingPlan {
        warmup_count: warmups,
        sample_count: samples,
        iterations_per_sample: Some(iterations),
        timeout_ms: None,
    }
}

fn call_case(kind: &str, arity: usize, tier: Tier) -> BaselineCase {
    let id = format!("call-{kind}-a{arity}-{}", tier.cli);
    BaselineCase {
        id,
        args: vec![
            "call".into(),
            "--kind".into(),
            kind.into(),
            "--arity".into(),
            arity.to_string(),
            "--jit-tier".into(),
            tier.cli.into(),
            "--iterations".into(),
            CALL_ITERATIONS.to_string(),
            "--samples".into(),
            CALL_SAMPLES.to_string(),
            "--warmup".into(),
            CALL_WARMUPS.to_string(),
        ],
        benchmark: BenchmarkIdentity {
            suite: "engine".into(),
            name: format!("call-{kind}-arity-{arity}"),
            parameters: parameters([
                ("kind", kind.to_owned()),
                ("arity", arity.to_string()),
                ("iterations", CALL_ITERATIONS.to_string()),
            ]),
        },
        configuration: configuration(
            ExecutionSurface::Vm,
            tier.policy,
            GcPolicy::Normal,
            RuntimeReuse::NotApplicable,
        ),
        sampling: sampling(CALL_WARMUPS, CALL_SAMPLES, u64::from(CALL_ITERATIONS)),
    }
}

fn module_case(entry: &str, reuse: RuntimeReuse, tier: Tier) -> BaselineCase {
    let reuse_cli = match reuse {
        RuntimeReuse::FreshPerSample => "fresh-per-sample",
        RuntimeReuse::ReusedAcrossSamples => "reused-across-samples",
        RuntimeReuse::NotApplicable => unreachable!("module cases always select runtime reuse"),
    };
    let warmups = if reuse == RuntimeReuse::FreshPerSample {
        0
    } else {
        MODULE_WARMUPS
    };
    let fixture = Path::new(entry)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("module");
    let family = if entry.contains("/package/") {
        "package"
    } else {
        "module"
    };
    BaselineCase {
        id: format!("{family}-{fixture}-{reuse_cli}-{}", tier.cli),
        args: vec![
            "module".into(),
            "--entry".into(),
            entry.into(),
            "--runtime-reuse".into(),
            reuse_cli.into(),
            "--jit-tier".into(),
            tier.cli.into(),
            "--samples".into(),
            MODULE_SAMPLES.to_string(),
            "--warmup".into(),
            warmups.to_string(),
        ],
        benchmark: BenchmarkIdentity {
            suite: "engine".into(),
            name: format!(
                "module-phases-{}",
                Path::new(entry)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("entry")
            ),
            parameters: parameters([("entry", entry.to_owned())]),
        },
        configuration: configuration(
            ExecutionSurface::Runtime,
            tier.policy,
            GcPolicy::Normal,
            reuse,
        ),
        sampling: sampling(warmups, MODULE_SAMPLES, 1),
    }
}

fn kernel_case(spec: KernelSpec, tier: Tier) -> BaselineCase {
    BaselineCase {
        id: format!("kernel-{}-{}", spec.slug, tier.cli),
        args: vec![
            "kernel".into(),
            "--source".into(),
            spec.source.into(),
            "--function".into(),
            "engineKernel".into(),
            "--expected".into(),
            spec.expected.into(),
            "--jit-tier".into(),
            tier.cli.into(),
            "--samples".into(),
            spec.samples.to_string(),
            "--warmup".into(),
            spec.warmups.to_string(),
        ],
        benchmark: BenchmarkIdentity {
            suite: "engine".into(),
            name: format!("kernel-{}", spec.slug),
            parameters: parameters([
                ("source", spec.source.into()),
                ("function", "engineKernel".into()),
                ("expected", spec.expected.into()),
            ]),
        },
        configuration: configuration(
            ExecutionSurface::Vm,
            tier.policy,
            GcPolicy::Normal,
            RuntimeReuse::NotApplicable,
        ),
        sampling: sampling(spec.warmups, spec.samples, 1),
    }
}

fn baseline_cases() -> Vec<BaselineCase> {
    let mut cases = Vec::with_capacity(30);
    for arity in [0, 4] {
        for tier in TIERS {
            cases.push(call_case("bytecode", arity, tier));
        }
    }
    for tier in TIERS {
        cases.push(call_case("host", 1, tier));
    }
    for spec in KERNELS {
        for tier in TIERS {
            cases.push(kernel_case(spec, tier));
        }
    }
    cases.push(BaselineCase {
        id: "jit-compile-engine-target".into(),
        args: vec![
            "jit-compile".into(),
            "--source".into(),
            "benchmarks/fixtures/engine/jit-compile.js".into(),
            "--function".into(),
            "engineJitTarget".into(),
            "--expected".into(),
            "3300".into(),
            "--samples".into(),
            COMPILE_SAMPLES.to_string(),
            "--warmup".into(),
            COMPILE_WARMUPS.to_string(),
        ],
        benchmark: BenchmarkIdentity {
            suite: "engine".into(),
            name: "jit-compile-engineJitTarget".into(),
            parameters: parameters([
                ("source", "benchmarks/fixtures/engine/jit-compile.js".into()),
                ("function", "engineJitTarget".into()),
                ("expected", "3300".into()),
            ]),
        },
        configuration: configuration(
            ExecutionSurface::Vm,
            JitPolicy::Template,
            GcPolicy::Normal,
            RuntimeReuse::NotApplicable,
        ),
        sampling: sampling(COMPILE_WARMUPS, COMPILE_SAMPLES, 1),
    });
    cases.push(BaselineCase {
        id: "memory-forced-full".into(),
        args: vec![
            "memory".into(),
            "--iterations".into(),
            MEMORY_ITERATIONS.to_string(),
            "--samples".into(),
            MEMORY_SAMPLES.to_string(),
        ],
        benchmark: BenchmarkIdentity {
            suite: "engine".into(),
            name: "memory-allocation-churn-forced-full".into(),
            parameters: parameters([("iterations", MEMORY_ITERATIONS.to_string())]),
        },
        configuration: configuration(
            ExecutionSurface::Vm,
            JitPolicy::Interpreter,
            GcPolicy::ForcedFull,
            RuntimeReuse::NotApplicable,
        ),
        sampling: sampling(0, MEMORY_SAMPLES, u64::from(MEMORY_ITERATIONS)),
    });
    for tier in TIERS {
        cases.push(module_case(
            "benchmarks/fixtures/engine/module-entry.mjs",
            RuntimeReuse::FreshPerSample,
            tier,
        ));
    }
    for tier in TIERS {
        cases.push(module_case(
            "benchmarks/fixtures/engine/module-entry.mjs",
            RuntimeReuse::ReusedAcrossSamples,
            tier,
        ));
    }
    cases.push(module_case(
        "benchmarks/fixtures/engine/package/entry.mjs",
        RuntimeReuse::FreshPerSample,
        TIERS[0],
    ));
    cases
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn command_output(root: &Path, program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = ProcessCommand::new(program)
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|error| format!("run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn command_text(root: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let output = command_output(root, program, args)?;
    let text = String::from_utf8_lossy(&output).trim().to_owned();
    if text.is_empty() {
        Err(format!("{program} returned empty output"))
    } else {
        Ok(text)
    }
}

fn repo_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| format!("current directory: {error}"))?;
    command_text(&cwd, "git", &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

fn git_head(root: &Path) -> Result<String, String> {
    command_text(root, "git", &["rev-parse", "HEAD"])
}

fn git_clean(root: &Path) -> Result<bool, String> {
    command_output(
        root,
        "git",
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    )
    .map(|output| output.is_empty())
}

fn require_same_clean_head(root: &Path, expected: &str) -> Result<(), String> {
    let actual = git_head(root)?;
    if actual != expected {
        return Err(format!("HEAD changed from {expected} to {actual}"));
    }
    if !git_clean(root)? {
        return Err("worktree is not clean".into());
    }
    Ok(())
}

fn reject_perturbing_environment() -> Result<(), String> {
    let variables = std::env::vars()
        .filter(|(name, value)| {
            !value.is_empty()
                && (name.starts_with("OTTER_JIT")
                    || name.starts_with("OTTER_GC")
                    || name == "RUST_LOG")
        })
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    if variables.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "unset benchmark-perturbing environment variables: {}",
            variables.join(", ")
        ))
    }
}

fn absolute_from(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    }
}

fn require_ignored(root: &Path, path: &Path) -> Result<(), String> {
    let status = ProcessCommand::new("git")
        .current_dir(root)
        .args(["check-ignore", "-q", "--no-index"])
        .arg(path)
        .status()
        .map_err(|error| format!("git check-ignore: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "capture output must be ignored by git: {}",
            path.display()
        ))
    }
}

fn build_engine(root: &Path) -> Result<(), String> {
    let status = ProcessCommand::new("cargo")
        .current_dir(root)
        .args([
            "build",
            "--locked",
            "--release",
            "-p",
            "otter-benchmark",
            "--features",
            "engine",
            "--bin",
            "otter-engine-benchmark",
        ])
        .status()
        .map_err(|error| format!("build engine benchmark: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("engine benchmark build exited with {status}"))
    }
}

fn run_child_to_files(
    root: &Path,
    argv: &[String],
    stdout_path: &Path,
    stderr_path: &Path,
    timeout_ms: u64,
) -> std::io::Result<ChildRun> {
    let stdout = File::create(stdout_path)?;
    let stderr = File::create(stderr_path)?;
    let mut child = ProcessCommand::new(&argv[0])
        .current_dir(root)
        .args(&argv[1..])
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()?;
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(ChildRun {
                status,
                timed_out: false,
            });
        }
        if started.elapsed() >= timeout {
            if let Err(error) = child.kill() {
                if let Some(status) = child.try_wait()? {
                    return Ok(ChildRun {
                        status,
                        timed_out: false,
                    });
                }
                return Err(error);
            }
            return child.wait().map(|status| ChildRun {
                status,
                timed_out: true,
            });
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn parse_record(bytes: &[u8]) -> Result<BenchmarkResult, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("benchmark JSON: {error}"))
}

fn validate_record(
    record: &BenchmarkResult,
    case: &BaselineCase,
    expected_commit: &str,
    expected_argv: &[String],
) -> Result<(), String> {
    if let Some(error) = record.contract_error() {
        return Err(format!("contract: {error}"));
    }
    if record.benchmark != case.benchmark {
        return Err(format!(
            "benchmark identity mismatch: {:?} != {:?}",
            record.benchmark, case.benchmark
        ));
    }
    if record.configuration != case.configuration {
        return Err(format!(
            "configuration mismatch: {:?} != {:?}",
            record.configuration, case.configuration
        ));
    }
    if record.sampling != case.sampling {
        return Err(format!(
            "sampling mismatch: {:?} != {:?}",
            record.sampling, case.sampling
        ));
    }
    if record.command != expected_argv {
        return Err(format!(
            "argv mismatch: {:?} != {expected_argv:?}",
            record.command
        ));
    }
    if record.provenance.commit != expected_commit {
        return Err(format!(
            "record commit {} != {expected_commit}",
            record.provenance.commit
        ));
    }
    if record.provenance.dirty {
        return Err("record reports a dirty worktree".into());
    }
    if record.provenance.build_profile != "release" {
        return Err(format!(
            "record build profile {:?} is not release",
            record.provenance.build_profile
        ));
    }
    if record.provenance.captured_at_unix_ms == 0 {
        return Err("record capture timestamp is zero".into());
    }
    if record.sampling.timeout_ms.is_some() {
        return Err("engine record must not claim the outer watchdog".into());
    }
    Ok(())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|error| format!("serialize JSON: {error}"))?;
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(|error| format!("write {}: {error}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
}

fn relative_path(kind: &str, ordinal: usize, id: &str, extension: &str) -> String {
    format!("{kind}/{ordinal:02}-{id}.{extension}")
}

fn enum_text<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "\"unknown\"".into())
        .trim_matches('"')
        .to_owned()
}

fn metric_value(value: MetricValue) -> String {
    match value {
        MetricValue::Integer(value) => value.to_string(),
        MetricValue::Decimal(value) => value.to_string(),
    }
}

fn escape_table(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

fn render_summary(manifest: &CaptureManifest, records: &[Option<BenchmarkResult>]) -> String {
    let first = records.iter().flatten().next();
    let mut output = String::new();
    output.push_str("# Otter Engine Baseline\n\n");
    output.push_str(&format!("- Commit: `{}`\n", manifest.commit));
    output.push_str(&format!(
        "- Outer watchdog: `{}` ms (capture-only; record timeout remains null)\n",
        manifest.outer_timeout_ms
    ));
    if let Some(record) = first {
        output.push_str(&format!(
            "- Platform: `{}` / `{}` / `{}` / `{}`\n",
            escape_table(&record.provenance.platform.os),
            escape_table(&record.provenance.platform.arch),
            escape_table(&record.provenance.platform.kernel),
            escape_table(&record.provenance.platform.cpu)
        ));
        output.push_str(&format!(
            "- Toolchain: `{}`\n",
            escape_table(&record.provenance.rust_toolchain)
        ));
    }
    output.push('\n');
    output.push_str(
        "| # | Capture id | Benchmark | JIT | Reuse | Samples | Primary | Unit | Status | Eligible |\n",
    );
    output.push_str("| ---: | --- | --- | --- | --- | ---: | ---: | --- | --- | --- |\n");
    for (index, run) in manifest.runs.iter().enumerate() {
        let record = records.get(index).and_then(Option::as_ref);
        let benchmark = record
            .map(|record| record.benchmark.name.as_str())
            .unwrap_or("-");
        let jit = record
            .map(|record| enum_text(&record.configuration.jit_policy))
            .unwrap_or_else(|| "-".into());
        let reuse = record
            .map(|record| enum_text(&record.configuration.runtime_reuse))
            .unwrap_or_else(|| "-".into());
        let samples = record
            .map(|record| record.sampling.sample_count.to_string())
            .unwrap_or_else(|| "-".into());
        let primary = record.and_then(|record| {
            record
                .metrics
                .iter()
                .find(|metric| metric.role == MetricRole::Primary)
        });
        let primary_value = primary
            .map(|metric| metric_value(metric.aggregate.value))
            .unwrap_or_else(|| "-".into());
        let unit = primary
            .map(|metric| enum_text(&metric.unit))
            .unwrap_or_else(|| "-".into());
        let status = record
            .map(|record| enum_text(&record.outcome.status))
            .unwrap_or_else(|| enum_text(&run.status));
        let eligible = record.is_some_and(BenchmarkResult::is_baseline_eligible);
        output.push_str(&format!(
            "| {} | `{}` | `{}` | `{}` | `{}` | {} | {} | `{}` | `{}` | {} |\n",
            index + 1,
            escape_table(&run.id),
            escape_table(benchmark),
            escape_table(&jit),
            escape_table(&reuse),
            samples,
            primary_value,
            escape_table(&unit),
            escape_table(&status),
            if eligible { "yes" } else { "no" }
        ));
    }
    output
}

fn manifest_path(capture_dir: &Path) -> PathBuf {
    capture_dir.join("capture.json")
}

fn engine_argv0() -> String {
    format!(
        "target/release/otter-engine-benchmark{}",
        std::env::consts::EXE_SUFFIX
    )
}

fn capture(output_root: PathBuf, outer_timeout_ms: u64) -> Result<(PathBuf, bool), String> {
    if current_build_profile() != "release" {
        return Err("run otter-engine-baseline with --release".into());
    }
    if outer_timeout_ms == 0 {
        return Err("outer timeout must be greater than zero".into());
    }
    reject_perturbing_environment()?;
    let root = repo_root()?;
    std::env::set_current_dir(&root).map_err(|error| format!("enter repo root: {error}"))?;
    let commit = git_head(&root)?;
    require_same_clean_head(&root, &commit)?;
    build_engine(&root)?;
    require_same_clean_head(&root, &commit)?;

    let engine_argv0 = engine_argv0();
    let engine_path = root.join(&engine_argv0);
    if !engine_path.is_file() {
        return Err(format!("missing engine binary {}", engine_path.display()));
    }
    let output_root = absolute_from(&root, &output_root);
    require_ignored(&root, &output_root)?;
    let short_commit = commit.get(..12).unwrap_or(&commit);
    let capture_dir = output_root.join(format!("engine-{short_commit}-{}", unix_ms()));
    if capture_dir.exists() {
        return Err(format!(
            "capture directory already exists: {}",
            capture_dir.display()
        ));
    }
    fs::create_dir_all(capture_dir.join("records"))
        .and_then(|_| fs::create_dir_all(capture_dir.join("raw")))
        .map_err(|error| format!("create capture directory: {error}"))?;

    let cases = baseline_cases();
    let mut manifest = CaptureManifest {
        commit: commit.clone(),
        captured_at_unix_ms: unix_ms(),
        outer_timeout_ms,
        engine_argv0: engine_argv0.clone(),
        complete: false,
        postflight_error: None,
        runs: Vec::with_capacity(cases.len()),
    };
    write_json(&manifest_path(&capture_dir), &manifest)?;
    let mut records = Vec::with_capacity(cases.len());

    for (index, case) in cases.iter().enumerate() {
        let ordinal = index + 1;
        let stdout_rel = relative_path("raw", ordinal, &case.id, "stdout");
        let stderr_rel = relative_path("raw", ordinal, &case.id, "stderr");
        let record_rel = relative_path("records", ordinal, &case.id, "json");
        let stdout_path = capture_dir.join(&stdout_rel);
        let stderr_path = capture_dir.join(&stderr_rel);
        let argv = case.argv(&engine_argv0);
        let result = run_child_to_files(&root, &argv, &stdout_path, &stderr_path, outer_timeout_ms);
        let mut run = CaptureRun {
            id: case.id.clone(),
            argv: argv.clone(),
            status: CaptureStatus::SpawnFailed,
            process_exit_code: None,
            record_path: None,
            stdout_path: stdout_rel,
            stderr_path: stderr_rel,
            error: None,
        };
        let mut parsed = None;
        match result {
            Err(error) => {
                run.error = Some(error.to_string());
            }
            Ok(child) if child.timed_out => {
                run.status = CaptureStatus::OuterTimeout;
                run.process_exit_code = child.status.code();
                run.error = Some(format!(
                    "outer watchdog expired after {outer_timeout_ms} ms"
                ));
            }
            Ok(child) => {
                run.process_exit_code = child.status.code();
                let stdout = fs::read(&stdout_path)
                    .map_err(|error| format!("read {}: {error}", stdout_path.display()))?;
                match parse_record(&stdout).and_then(|record| {
                    validate_record(&record, case, &commit, &argv)?;
                    Ok(record)
                }) {
                    Ok(record) => {
                        fs::write(capture_dir.join(&record_rel), &stdout)
                            .map_err(|error| format!("write record {record_rel}: {error}"))?;
                        run.status = CaptureStatus::Completed;
                        run.record_path = Some(record_rel);
                        parsed = Some(record);
                    }
                    Err(error) => {
                        run.status = CaptureStatus::InvalidRecord;
                        run.error = Some(error);
                    }
                }
            }
        }
        manifest.runs.push(run);
        records.push(parsed);
        write_json(&manifest_path(&capture_dir), &manifest)?;
    }

    if let Err(error) = require_same_clean_head(&root, &commit) {
        manifest.postflight_error = Some(error);
    } else {
        manifest.complete = true;
    }
    write_json(&manifest_path(&capture_dir), &manifest)?;
    fs::write(
        capture_dir.join("SUMMARY.md"),
        render_summary(&manifest, &records),
    )
    .map_err(|error| format!("write capture summary: {error}"))?;

    let publishable = validate_capture(&root, &capture_dir).is_ok();
    Ok((capture_dir, publishable))
}

fn validate_capture(
    root: &Path,
    capture_dir: &Path,
) -> Result<(CaptureManifest, Vec<Option<BenchmarkResult>>), String> {
    let manifest: CaptureManifest = read_json(&manifest_path(capture_dir))?;
    if !manifest.complete || manifest.postflight_error.is_some() {
        return Err("capture did not complete a clean postflight".into());
    }
    let engine_argv0 = engine_argv0();
    if manifest.engine_argv0 != engine_argv0 {
        return Err(format!(
            "capture engine argv0 {:?} != {:?}",
            manifest.engine_argv0, engine_argv0
        ));
    }
    require_same_clean_head(root, &manifest.commit)?;
    let cases = baseline_cases();
    if manifest.runs.len() != cases.len() {
        return Err(format!(
            "capture has {} runs, expected {}",
            manifest.runs.len(),
            cases.len()
        ));
    }
    let mut records = Vec::with_capacity(cases.len());
    let mut common_platform = None;
    let mut common_toolchain = None;
    for (index, (run, case)) in manifest.runs.iter().zip(&cases).enumerate() {
        if run.id != case.id {
            return Err(format!("capture id {:?} != {:?}", run.id, case.id));
        }
        let expected_argv = case.argv(&engine_argv0);
        if run.argv != expected_argv {
            return Err(format!("capture argv mismatch for {}", case.id));
        }
        if run.status != CaptureStatus::Completed
            || run.process_exit_code != Some(0)
            || run.error.is_some()
        {
            return Err(format!("capture run {} is not successful", case.id));
        }
        let ordinal = index + 1;
        let expected_record_path = relative_path("records", ordinal, &case.id, "json");
        let expected_stdout_path = relative_path("raw", ordinal, &case.id, "stdout");
        let expected_stderr_path = relative_path("raw", ordinal, &case.id, "stderr");
        if run.record_path.as_deref() != Some(expected_record_path.as_str())
            || run.stdout_path != expected_stdout_path
            || run.stderr_path != expected_stderr_path
        {
            return Err(format!("capture paths mismatch for {}", case.id));
        }
        let record_rel = run
            .record_path
            .as_ref()
            .ok_or_else(|| format!("capture run {} has no record", case.id))?;
        let bytes = fs::read(capture_dir.join(record_rel))
            .map_err(|error| format!("read record {record_rel}: {error}"))?;
        let raw_stdout = fs::read(capture_dir.join(&run.stdout_path))
            .map_err(|error| format!("read raw stdout for {}: {error}", case.id))?;
        if raw_stdout != bytes {
            return Err(format!(
                "raw stdout and benchmark record differ for {}",
                case.id
            ));
        }
        let raw_stderr = fs::read(capture_dir.join(&run.stderr_path))
            .map_err(|error| format!("read raw stderr for {}: {error}", case.id))?;
        if !raw_stderr.is_empty() {
            return Err(format!("successful run {} wrote to stderr", case.id));
        }
        let record = parse_record(&bytes)?;
        validate_record(&record, case, &manifest.commit, &expected_argv)?;
        if !record.is_scoreable() || !record.is_baseline_eligible() {
            return Err(format!("capture run {} is not baseline eligible", case.id));
        }
        match &common_platform {
            Some(platform) if platform != &record.provenance.platform => {
                return Err(format!("platform changed at {}", case.id));
            }
            None => common_platform = Some(record.provenance.platform.clone()),
            _ => {}
        }
        match &common_toolchain {
            Some(toolchain) if toolchain != &record.provenance.rust_toolchain => {
                return Err(format!("toolchain changed at {}", case.id));
            }
            None => common_toolchain = Some(record.provenance.rust_toolchain.clone()),
            _ => {}
        }
        records.push(Some(record));
    }
    let expected_summary = render_summary(&manifest, &records);
    let actual_summary = fs::read_to_string(capture_dir.join("SUMMARY.md"))
        .map_err(|error| format!("read capture summary: {error}"))?;
    if actual_summary != expected_summary {
        return Err("capture summary is not derived from its records".into());
    }
    Ok((manifest, records))
}

fn copy_file(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    fs::copy(from, to)
        .map(|_| ())
        .map_err(|error| format!("copy {} to {}: {error}", from.display(), to.display()))
}

fn copy_capture_snapshot(capture_dir: &Path, snapshot_dir: &Path) -> Result<(), String> {
    copy_file(&manifest_path(capture_dir), &manifest_path(snapshot_dir))?;
    copy_file(
        &capture_dir.join("SUMMARY.md"),
        &snapshot_dir.join("SUMMARY.md"),
    )?;
    for (index, case) in baseline_cases().iter().enumerate() {
        let ordinal = index + 1;
        for (directory, extension) in [("records", "json"), ("raw", "stdout"), ("raw", "stderr")] {
            let relative = relative_path(directory, ordinal, &case.id, extension);
            copy_file(&capture_dir.join(&relative), &snapshot_dir.join(&relative))?;
        }
    }
    Ok(())
}

fn install_published_baseline(
    temporary: &Path,
    output: &Path,
    previous: &Path,
) -> Result<(), String> {
    if previous.exists() {
        return Err(format!(
            "previous-baseline staging path exists: {}",
            previous.display()
        ));
    }
    if !output.exists() {
        return fs::rename(temporary, output).map_err(|error| {
            format!(
                "publish {} to {}: {error}",
                temporary.display(),
                output.display()
            )
        });
    }
    if !output.is_dir() {
        return Err(format!(
            "published baseline is not a directory: {}",
            output.display()
        ));
    }

    fs::rename(output, previous).map_err(|error| {
        format!(
            "stage previous baseline {} at {}: {error}",
            output.display(),
            previous.display()
        )
    })?;
    if let Err(error) = fs::rename(temporary, output) {
        return match fs::rename(previous, output) {
            Ok(()) => Err(format!(
                "publish {} to {}: {error}; previous baseline restored",
                temporary.display(),
                output.display()
            )),
            Err(rollback_error) => Err(format!(
                "publish {} to {}: {error}; restore {} failed: {rollback_error}",
                temporary.display(),
                output.display(),
                previous.display()
            )),
        };
    }
    if let Err(error) = fs::remove_dir_all(previous) {
        eprintln!(
            "warning: published {}, but retained previous baseline {}: {error}",
            output.display(),
            previous.display()
        );
    }
    Ok(())
}

fn publish(capture: PathBuf) -> Result<PathBuf, String> {
    let root = repo_root()?;
    std::env::set_current_dir(&root).map_err(|error| format!("enter repo root: {error}"))?;
    let capture_dir = absolute_from(&root, &capture);
    let output = root.join("benchmarks/baseline");
    let publish_nonce = unix_ms();
    let temporary = root.join("benchmarks/results").join(format!(
        ".baseline-publish-{}-{}",
        std::process::id(),
        publish_nonce
    ));
    let previous = root.join("benchmarks/results").join(format!(
        ".baseline-previous-{}-{}",
        std::process::id(),
        publish_nonce
    ));
    if temporary.exists() {
        return Err(format!(
            "temporary publish path exists: {}",
            temporary.display()
        ));
    }
    require_ignored(&root, &temporary)?;
    fs::create_dir_all(&temporary)
        .map_err(|error| format!("create {}: {error}", temporary.display()))?;
    let publish_result = (|| {
        copy_capture_snapshot(&capture_dir, &temporary)?;
        validate_capture(&root, &temporary)?;
        Ok::<(), String>(())
    })();
    if let Err(error) = publish_result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    if let Err(error) = require_ignored(&root, &previous) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    install_published_baseline(&temporary, &output, &previous)?;
    Ok(output)
}

fn main() {
    let args = Args::parse();
    match args.command {
        Command::Capture {
            output_root,
            outer_timeout_ms,
        } => match capture(output_root, outer_timeout_ms) {
            Ok((path, true)) => println!("{}", path.display()),
            Ok((path, false)) => {
                eprintln!(
                    "capture retained non-publishable observations at {}",
                    path.display()
                );
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("capture failed: {error}");
                std::process::exit(1);
            }
        },
        Command::Publish { capture } => match publish(capture) {
            Ok(path) => println!("{}", path.display()),
            Err(error) => {
                eprintln!("publish failed: {error}");
                std::process::exit(1);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use otter_benchmark::{
        BenchmarkOutcome, BenchmarkProvenance, Metric, MetricDirection, MetricUnit, OutcomeStatus,
        Platform, Statistic,
    };

    fn record_for(case: &BaselineCase, argv: Vec<String>) -> BenchmarkResult {
        BenchmarkResult {
            benchmark: case.benchmark.clone(),
            provenance: BenchmarkProvenance {
                captured_at_unix_ms: 1,
                commit: "0123456789abcdef".into(),
                dirty: false,
                platform: Platform {
                    os: "macos".into(),
                    arch: "aarch64".into(),
                    kernel: "Darwin".into(),
                    cpu: "test".into(),
                },
                rust_toolchain: "rustc test".into(),
                build_profile: "release".into(),
            },
            configuration: case.configuration.clone(),
            sampling: case.sampling.clone(),
            metrics: vec![
                Metric::from_u64_samples(
                    "wall-time",
                    MetricUnit::Nanoseconds,
                    MetricDirection::LowerIsBetter,
                    MetricRole::Primary,
                    vec![1; case.sampling.sample_count as usize],
                    Statistic::Median,
                )
                .unwrap(),
            ],
            outcome: BenchmarkOutcome {
                status: OutcomeStatus::Validated,
                validation_marker: Some("ok".into()),
                process_exit_code: None,
                failure: None,
            },
            command: argv,
        }
    }

    #[test]
    fn matrix_is_exact_engine_only_and_unique() {
        let cases = baseline_cases();
        assert_eq!(cases.len(), 30);
        let ids = cases
            .iter()
            .map(|case| case.id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), cases.len());
        let text = cases
            .iter()
            .flat_map(|case| case.args.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!text.contains("node"));
        assert!(!text.contains("phase0"));
        assert!(!text.contains("baseline"));
        assert!(!text.contains("schema"));
        assert_eq!(
            cases
                .iter()
                .filter(|case| case.args.first().is_some_and(|arg| arg == "call"))
                .count(),
            9
        );
        assert_eq!(
            cases
                .iter()
                .filter(|case| case.args.first().is_some_and(|arg| arg == "module"))
                .count(),
            7
        );
        assert_eq!(
            cases
                .iter()
                .filter(|case| case.args.first().is_some_and(|arg| arg == "kernel"))
                .count(),
            12
        );
    }

    #[test]
    fn record_validation_rejects_dirty_wrong_argv_and_inner_timeout() {
        let case = &baseline_cases()[0];
        let argv = case.argv("target/release/otter-engine-benchmark");
        let mut record = record_for(case, argv.clone());
        assert!(validate_record(&record, case, "0123456789abcdef", &argv).is_ok());
        record.provenance.dirty = true;
        assert!(validate_record(&record, case, "0123456789abcdef", &argv).is_err());
        record.provenance.dirty = false;
        record.command.push("extra".into());
        assert!(validate_record(&record, case, "0123456789abcdef", &argv).is_err());
        record.command = argv.clone();
        record.sampling.timeout_ms = Some(1);
        assert!(validate_record(&record, case, "0123456789abcdef", &argv).is_err());
    }

    #[test]
    fn parser_rejects_trailing_output_and_manifest_has_no_version_key() {
        let case = &baseline_cases()[0];
        let argv = case.argv("engine");
        let record = record_for(case, argv);
        let mut bytes = serde_json::to_vec(&record).unwrap();
        bytes.extend_from_slice(b"\nnot-json");
        assert!(parse_record(&bytes).is_err());

        let manifest = CaptureManifest {
            commit: "head".into(),
            captured_at_unix_ms: 1,
            outer_timeout_ms: 10,
            engine_argv0: "engine".into(),
            complete: false,
            postflight_error: None,
            runs: Vec::new(),
        };
        let value = serde_json::to_value(manifest).unwrap();
        assert!(value.get("version").is_none());
        assert!(value.get("schemaVersion").is_none());
    }

    #[test]
    fn publish_has_one_fixed_destination_and_engine_binary() {
        assert!(
            Args::try_parse_from([
                "otter-engine-baseline",
                "publish",
                "--capture",
                "capture",
                "--output",
                "elsewhere",
            ])
            .is_err()
        );
        assert_eq!(
            engine_argv0(),
            format!(
                "target/release/otter-engine-benchmark{}",
                std::env::consts::EXE_SUFFIX
            )
        );
    }

    #[test]
    fn published_baseline_replaces_current_and_rolls_back_on_failure() {
        let root = std::env::temp_dir().join(format!(
            "otter-baseline-publish-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let output = root.join("baseline");
        let temporary = root.join("new");
        let previous = root.join("previous");
        fs::create_dir_all(&output).unwrap();
        fs::create_dir_all(&temporary).unwrap();
        fs::write(output.join("marker"), "old").unwrap();
        fs::write(temporary.join("marker"), "new").unwrap();

        install_published_baseline(&temporary, &output, &previous).unwrap();
        assert_eq!(fs::read_to_string(output.join("marker")).unwrap(), "new");
        assert!(!temporary.exists());
        assert!(!previous.exists());

        let missing = root.join("missing");
        let error = install_published_baseline(&missing, &output, &previous).unwrap_err();
        assert!(error.contains("previous baseline restored"));
        assert_eq!(fs::read_to_string(output.join("marker")).unwrap(), "new");
        assert!(!previous.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn outer_timeout_is_bounded_and_does_not_fabricate_a_record() {
        let root = std::env::temp_dir().join(format!(
            "otter-baseline-timeout-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        let stdout = root.join("stdout");
        let stderr = root.join("stderr");
        let argv = vec!["sh".into(), "-c".into(), "sleep 2".into()];
        let started = Instant::now();
        let child = run_child_to_files(&root, &argv, &stdout, &stderr, 50).unwrap();
        assert!(child.timed_out);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(parse_record(&fs::read(stdout).unwrap()).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn summary_is_deterministic_and_derived() {
        let cases = baseline_cases();
        let engine = "target/release/otter-engine-benchmark";
        let records = cases
            .iter()
            .map(|case| Some(record_for(case, case.argv(engine))))
            .collect::<Vec<_>>();
        let manifest = CaptureManifest {
            commit: "0123456789abcdef".into(),
            captured_at_unix_ms: 1,
            outer_timeout_ms: DEFAULT_OUTER_TIMEOUT_MS,
            engine_argv0: engine.into(),
            complete: true,
            postflight_error: None,
            runs: cases
                .iter()
                .enumerate()
                .map(|(index, case)| CaptureRun {
                    id: case.id.clone(),
                    argv: case.argv(engine),
                    status: CaptureStatus::Completed,
                    process_exit_code: Some(0),
                    record_path: Some(relative_path("records", index + 1, &case.id, "json")),
                    stdout_path: relative_path("raw", index + 1, &case.id, "stdout"),
                    stderr_path: relative_path("raw", index + 1, &case.id, "stderr"),
                    error: None,
                })
                .collect(),
        };
        let first = render_summary(&manifest, &records);
        let second = render_summary(&manifest, &records);
        assert_eq!(first, second);
        assert_eq!(first.matches("\n| ").count(), 32);
        assert!(first.contains("production-tiered"));
    }
}
