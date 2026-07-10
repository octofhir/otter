//! Process-isolated differential gate for Otter execution tiers and GC modes.
//!
//! # Contents
//! - Runs each committed corpus program through fresh `otter -p` processes.
//! - Compares completion/console output, thrown diagnostics, ordering, and exit.
//! - Emits a deterministic JSON report suitable for CI retention.
//!
//! # Invariants
//! - Interpreter-only is the semantic oracle.
//! - Every candidate has a wall-clock cap and is killed on timeout.
//! - GC-stress candidates also enable slot verification; any stale heap/root
//!   edge becomes a differential failure even when observable output survives.
//! - Corpus programs make otherwise-hidden final globals/effects part of their
//!   canonical completion object; no source rewriting or regex parsing occurs.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Compare Otter interpreter, baseline, and GC-stress processes")]
struct Args {
    /// Otter CLI binary. Defaults to target/release/otter.
    #[arg(long)]
    otter: Option<PathBuf>,
    /// Corpus directory.
    #[arg(long, default_value = "crates/otter-difftest/corpus")]
    corpus: PathBuf,
    /// Per-process wall-clock cap.
    #[arg(long, default_value_t = 20_000)]
    timeout_ms: u64,
    /// GC stress stride matrix, comma separated.
    #[arg(long, value_delimiter = ',', default_value = "1,4,16")]
    gc_strides: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Mode {
    InterpreterOnly,
    NormalTiering,
    ForcedBaseline,
    GcStress { stride: u32 },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct Observation {
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Serialize)]
struct CaseResult {
    case: String,
    passed: bool,
    oracle: Observation,
    candidates: Vec<(Mode, Observation)>,
    mismatch: Option<String>,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    seed: u64,
    otter: PathBuf,
    cases: Vec<CaseResult>,
    passed: usize,
    failed: usize,
}

fn temporary_output(case: &str, suffix: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "otter-difftest-{}-{stamp}-{}-{suffix}",
        std::process::id(),
        case.replace(['/', '\\'], "_")
    ))
}

fn run(otter: &Path, source: &str, case: &str, mode: &Mode, timeout: Duration) -> Observation {
    let stdout_path = temporary_output(case, "stdout");
    let stderr_path = temporary_output(case, "stderr");
    let stdout_file = File::create(&stdout_path).expect("create stdout capture");
    let stderr_file = File::create(&stderr_path).expect("create stderr capture");
    let mut command = Command::new(otter);
    command
        .arg("--timeout")
        .arg("0")
        .arg("-p")
        .arg(source)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .env_remove("OTTER_EXPERIMENTAL_OPTIMIZER")
        .env_remove("OTTER_GC_STRESS")
        .env_remove("OTTER_GC_VERIFY")
        .env_remove("OTTER_JIT_OSR_THRESHOLD");
    match mode {
        Mode::InterpreterOnly => {
            command.env("OTTER_JIT", "0");
        }
        Mode::NormalTiering => {
            command.env("OTTER_JIT", "1");
        }
        Mode::ForcedBaseline => {
            command
                .env("OTTER_JIT", "1")
                .env("OTTER_JIT_OSR_THRESHOLD", "1");
        }
        Mode::GcStress { stride } => {
            command
                .env("OTTER_JIT", "1")
                .env("OTTER_GC_STRESS", stride.to_string())
                .env("OTTER_GC_VERIFY", "1");
        }
    }
    let mut child = command.spawn().expect("spawn otter candidate");
    let started = Instant::now();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().expect("poll otter candidate") {
            break (Some(status), false);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let status = child.wait().ok();
            break (status, true);
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);
    Observation {
        exit_code: status.and_then(|value| value.code()),
        timed_out,
        stdout,
        stderr,
    }
}

fn corpus_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(root)
        .unwrap_or_else(|error| panic!("read corpus {}: {error}", root.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "js"))
        .collect();
    files.sort();
    files
}

fn main() {
    let args = Args::parse();
    let otter = args
        .otter
        .unwrap_or_else(|| PathBuf::from("target/release/otter"));
    let timeout = Duration::from_millis(args.timeout_ms);
    let mut modes = vec![Mode::NormalTiering, Mode::ForcedBaseline];
    modes.extend(
        args.gc_strides
            .into_iter()
            .map(|stride| Mode::GcStress { stride }),
    );

    let mut cases = Vec::new();
    for path in corpus_files(&args.corpus) {
        let source = fs::read_to_string(&path).expect("read corpus source");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let oracle = run(&otter, &source, &name, &Mode::InterpreterOnly, timeout);
        let candidates: Vec<_> = modes
            .iter()
            .cloned()
            .map(|mode| {
                let observation = run(&otter, &source, &name, &mode, timeout);
                (mode, observation)
            })
            .collect();
        let mismatch = candidates
            .iter()
            .find(|(_, observation)| observation != &oracle)
            .map(|(mode, observation)| format!("{mode:?} diverged: {observation:?}"));
        cases.push(CaseResult {
            case: name,
            passed: mismatch.is_none(),
            oracle,
            candidates,
            mismatch,
        });
    }
    let passed = cases.iter().filter(|case| case.passed).count();
    let failed = cases.len() - passed;
    let report = Report {
        schema_version: 1,
        seed: 0x004f_5454_4552,
        otter,
        cases,
        passed,
        failed,
    };
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if failed != 0 {
        std::process::exit(1);
    }
}
