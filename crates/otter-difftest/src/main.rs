//! Process-isolated differential gate for Otter execution tiers and GC modes.
//!
//! # Contents
//! - Runs each committed corpus program through fresh `otter -p` processes.
//! - Compares completion/console output, thrown diagnostics, ordering, and exit.
//! - Emits a deterministic JSON report suitable for CI retention.
//!
//! # Invariants
//! - Interpreter-only is the semantic oracle.
//! - The template candidate explicitly selects `--jitless`; environment
//!   hotness thresholds cannot substitute for a tier selection.
//! - Every candidate has a wall-clock cap and is killed on timeout.
//! - Empty corpora, timeouts and signal termination cannot count as agreement.
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
    TemplateBaseline,
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
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .env_remove("OTTER_GC_STRESS")
        .env_remove("OTTER_GC_VERIFY");
    // Tier selection is a CLI decision, not an environment one: the oracle asks
    // the binary for the interpreter alone (`--jitless` is the template
    // baseline tier, a code generator in its own right), so a renamed or
    // retired environment variable can never silently turn the oracle into
    // another tiered run.
    match mode {
        Mode::InterpreterOnly => {
            command.arg("--interpreter");
        }
        Mode::NormalTiering => {}
        Mode::TemplateBaseline => {
            command.arg("--jitless");
        }
        Mode::GcStress { stride } => {
            command
                .env("OTTER_GC_STRESS", stride.to_string())
                .env("OTTER_GC_VERIFY", "1");
        }
    }
    command.arg("--timeout").arg("0").arg("-p").arg(source);
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

fn mismatch(oracle: &Observation, candidates: &[(Mode, Observation)]) -> Option<String> {
    if oracle.timed_out || oracle.exit_code.is_none() {
        return Some(format!("Interpreter oracle did not complete: {oracle:?}"));
    }
    candidates.iter().find_map(|(mode, observation)| {
        if observation.timed_out || observation.exit_code.is_none() {
            Some(format!("{mode:?} did not complete: {observation:?}"))
        } else if observation != oracle {
            Some(format!("{mode:?} diverged: {observation:?}"))
        } else {
            None
        }
    })
}

fn main() {
    let args = Args::parse();
    let otter = args
        .otter
        .unwrap_or_else(|| PathBuf::from("target/release/otter"));
    let timeout = Duration::from_millis(args.timeout_ms);
    let mut modes = vec![Mode::NormalTiering, Mode::TemplateBaseline];
    modes.extend(
        args.gc_strides
            .into_iter()
            .map(|stride| Mode::GcStress { stride }),
    );

    let files = corpus_files(&args.corpus);
    if files.is_empty() {
        eprintln!("no JavaScript corpus files in {}", args.corpus.display());
        std::process::exit(2);
    }
    let mut cases = Vec::new();
    for path in files {
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
        let mismatch = mismatch(&oracle, &candidates);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(exit_code: Option<i32>, timed_out: bool) -> Observation {
        Observation {
            exit_code,
            timed_out,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    #[test]
    fn identical_timeouts_or_signal_terminations_do_not_pass() {
        for oracle in [observation(Some(0), true), observation(None, false)] {
            let candidates = vec![(Mode::NormalTiering, oracle.clone())];
            assert!(mismatch(&oracle, &candidates).is_some());
        }
    }

    #[test]
    fn candidate_failure_and_output_divergence_do_not_pass() {
        let oracle = observation(Some(0), false);
        let mut different_output = oracle.clone();
        different_output.stdout = "different result".into();
        for candidate in [
            observation(Some(0), true),
            observation(None, false),
            different_output,
        ] {
            assert!(mismatch(&oracle, &[(Mode::TemplateBaseline, candidate)]).is_some());
        }
    }

    #[test]
    fn matching_normal_and_thrown_completions_pass() {
        for exit_code in [0, 1] {
            let oracle = observation(Some(exit_code), false);
            assert!(mismatch(&oracle, &[(Mode::TemplateBaseline, oracle.clone())]).is_none());
        }
    }
}
