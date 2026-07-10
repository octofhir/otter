//! Command recorder for the Otter machine-readable benchmark schema.
//!
//! The optional `rss` feature enables explicit child-process RSS sampling.
//! Without `--rss-sample-ms`, command execution keeps the original direct
//! `Command::output` path and reports peak RSS as unavailable.

use std::process::{Command, ExitStatus};
use std::time::Instant;

#[cfg(feature = "rss")]
use std::io::Read;
#[cfg(feature = "rss")]
use std::process::Stdio;
#[cfg(feature = "rss")]
use std::time::Duration;

use clap::{Parser, ValueEnum};
use otter_benchmark::{
    BENCHMARK_RESULT_SCHEMA_VERSION, BenchmarkResult, CacheState, ExecutionMetrics, GcMode,
    JitMode, MemoryMetrics, RuntimeMode, ValidationStatus,
};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RuntimeModeArg {
    Cli,
    Embed,
    Vm,
    Package,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum JitModeArg {
    InterpreterOnly,
    Baseline,
    ForcedBaseline,
    ExperimentalOptimizer,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum GcModeArg {
    Normal,
    Stress,
    ForcedMinor,
    ForcedFull,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CacheStateArg {
    Cold,
    Warm,
    NotApplicable,
}

#[derive(Debug, Parser)]
#[command(about = "Run a command and emit an Otter benchmark-result JSON record")]
struct Args {
    #[arg(long)]
    name: String,
    #[arg(long, value_enum, default_value = "cli")]
    runtime_mode: RuntimeModeArg,
    #[arg(long, value_enum, default_value = "baseline")]
    jit_mode: JitModeArg,
    #[arg(long, value_enum, default_value = "normal")]
    gc_mode: GcModeArg,
    #[arg(long)]
    gc_stress_stride: Option<u32>,
    #[arg(long, value_enum, default_value = "not-applicable")]
    cache_state: CacheStateArg,
    #[arg(long)]
    validation_marker: Option<String>,
    #[arg(long, default_value = "release")]
    build_profile: String,
    #[cfg(feature = "rss")]
    #[arg(long, default_value_t = 0)]
    rss_sample_ms: u64,
    #[arg(required = true, last = true)]
    command: Vec<String>,
}

struct RecordedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    peak_rss_bytes: Option<u64>,
}

#[cfg(not(feature = "rss"))]
fn run_command(command: &[String]) -> std::io::Result<RecordedOutput> {
    Command::new(&command[0])
        .args(&command[1..])
        .output()
        .map(|output| RecordedOutput {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
            peak_rss_bytes: None,
        })
}

#[cfg(feature = "rss")]
fn run_command(command: &[String], rss_sample_ms: u64) -> std::io::Result<RecordedOutput> {
    if rss_sample_ms == 0 {
        return Command::new(&command[0])
            .args(&command[1..])
            .output()
            .map(|output| RecordedOutput {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
                peak_rss_bytes: None,
            });
    }

    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut stdout = stdout;
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut stderr = stderr;
        stderr.read_to_end(&mut output).map(|_| output)
    });

    let pid = sysinfo::Pid::from_u32(child.id());
    let mut system = sysinfo::System::new();
    let mut peak_rss_bytes = 0u64;
    let status = loop {
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if let Some(process) = system.process(pid) {
            peak_rss_bytes = peak_rss_bytes.max(process.memory());
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(rss_sample_ms));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| std::io::Error::other("stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| std::io::Error::other("stderr reader panicked"))??;
    Ok(RecordedOutput {
        status,
        stdout,
        stderr,
        peak_rss_bytes: (peak_rss_bytes > 0).then_some(peak_rss_bytes),
    })
}

fn text(command: &str, args: &[&str]) -> String {
    Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into())
}

fn main() {
    let args = Args::parse();
    let started = Instant::now();
    #[cfg(feature = "rss")]
    let output = run_command(&args.command, args.rss_sample_ms);
    #[cfg(not(feature = "rss"))]
    let output = run_command(&args.command);
    let wall_time_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;

    let (exit_code, stdout, success, failure, peak_rss_bytes) = match output {
        Ok(output) => (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            output.status.success(),
            (!output.status.success())
                .then(|| String::from_utf8_lossy(&output.stderr).into_owned()),
            output.peak_rss_bytes,
        ),
        Err(error) => (None, String::new(), false, Some(error.to_string()), None),
    };
    let validation = match &args.validation_marker {
        Some(marker) if success && stdout.contains(marker) => ValidationStatus::Validated,
        Some(_) => ValidationStatus::Failed,
        None if success => ValidationStatus::Unvalidated,
        None => ValidationStatus::Failed,
    };

    let platform = format!(
        "{} {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        text("uname", &["-r"])
    );
    let result = BenchmarkResult {
        schema_version: BENCHMARK_RESULT_SCHEMA_VERSION,
        benchmark: args.name,
        commit: text("git", &["rev-parse", "HEAD"]),
        platform,
        toolchain: text("rustc", &["-Vv"]),
        build_profile: args.build_profile,
        runtime_mode: match args.runtime_mode {
            RuntimeModeArg::Cli => RuntimeMode::Cli,
            RuntimeModeArg::Embed => RuntimeMode::Embed,
            RuntimeModeArg::Vm => RuntimeMode::Vm,
            RuntimeModeArg::Package => RuntimeMode::Package,
        },
        jit_mode: match args.jit_mode {
            JitModeArg::InterpreterOnly => JitMode::InterpreterOnly,
            JitModeArg::Baseline => JitMode::Baseline,
            JitModeArg::ForcedBaseline => JitMode::ForcedBaseline,
            JitModeArg::ExperimentalOptimizer => JitMode::ExperimentalOptimizer,
        },
        gc_mode: match args.gc_mode {
            GcModeArg::Normal => GcMode::Normal,
            GcModeArg::Stress => GcMode::Stress,
            GcModeArg::ForcedMinor => GcMode::ForcedMinor,
            GcModeArg::ForcedFull => GcMode::ForcedFull,
        },
        gc_stress_stride: args.gc_stress_stride,
        cache_state: match args.cache_state {
            CacheStateArg::Cold => CacheState::Cold,
            CacheStateArg::Warm => CacheState::Warm,
            CacheStateArg::NotApplicable => CacheState::NotApplicable,
        },
        execution: ExecutionMetrics {
            wall_time_ns,
            ..ExecutionMetrics::default()
        },
        memory: MemoryMetrics {
            peak_rss_bytes,
            ..MemoryMetrics::default()
        },
        exit_code,
        success,
        validation,
        validation_marker: args.validation_marker,
        command: args.command,
        failure,
    };
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
    if !result.is_scoreable() {
        std::process::exit(1);
    }
}
