//! External-command recorder for the live Otter benchmark contract.
//!
//! # Contents
//! - Runs one child command and records its exact configuration and outcome.
//! - Optionally enforces a timeout, samples RSS, and records a suite score.
//!
//! # Invariants
//! - The ordinary no-timeout/no-RSS path remains a direct `Command::output`.
//! - A validation marker is required for a scoreable child-process result.
//! - Process exit codes are recorded only for the child process.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use otter_benchmark::{
    BenchmarkConfiguration, BenchmarkFailure, BenchmarkIdentity, BenchmarkOutcome,
    BenchmarkProvenance, BenchmarkResult, CacheState, ExecutionSurface, FailureKind, GcPolicy,
    JitPolicy, Metric, MetricDirection, MetricRole, MetricUnit, OutcomeStatus, RuntimeReuse,
    SamplingPlan, Statistic,
};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ExecutionSurfaceArg {
    Vm,
    Runtime,
    CliProcess,
    ExternalProcess,
}

impl From<ExecutionSurfaceArg> for ExecutionSurface {
    fn from(value: ExecutionSurfaceArg) -> Self {
        match value {
            ExecutionSurfaceArg::Vm => Self::Vm,
            ExecutionSurfaceArg::Runtime => Self::Runtime,
            ExecutionSurfaceArg::CliProcess => Self::CliProcess,
            ExecutionSurfaceArg::ExternalProcess => Self::ExternalProcess,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum JitPolicyArg {
    Interpreter,
    Template,
    ProductionTiered,
    NotApplicable,
}

impl From<JitPolicyArg> for JitPolicy {
    fn from(value: JitPolicyArg) -> Self {
        match value {
            JitPolicyArg::Interpreter => Self::Interpreter,
            JitPolicyArg::Template => Self::Template,
            JitPolicyArg::ProductionTiered => Self::ProductionTiered,
            JitPolicyArg::NotApplicable => Self::NotApplicable,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum GcPolicyArg {
    Normal,
    Stress,
    ForcedMinor,
    ForcedFull,
    NotApplicable,
}

impl From<GcPolicyArg> for GcPolicy {
    fn from(value: GcPolicyArg) -> Self {
        match value {
            GcPolicyArg::Normal => Self::Normal,
            GcPolicyArg::Stress => Self::Stress,
            GcPolicyArg::ForcedMinor => Self::ForcedMinor,
            GcPolicyArg::ForcedFull => Self::ForcedFull,
            GcPolicyArg::NotApplicable => Self::NotApplicable,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RuntimeReuseArg {
    FreshPerSample,
    ReusedAcrossSamples,
    NotApplicable,
}

impl From<RuntimeReuseArg> for RuntimeReuse {
    fn from(value: RuntimeReuseArg) -> Self {
        match value {
            RuntimeReuseArg::FreshPerSample => Self::FreshPerSample,
            RuntimeReuseArg::ReusedAcrossSamples => Self::ReusedAcrossSamples,
            RuntimeReuseArg::NotApplicable => Self::NotApplicable,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CacheStateArg {
    Cold,
    Warm,
    NotApplicable,
}

impl From<CacheStateArg> for CacheState {
    fn from(value: CacheStateArg) -> Self {
        match value {
            CacheStateArg::Cold => Self::Cold,
            CacheStateArg::Warm => Self::Warm,
            CacheStateArg::NotApplicable => Self::NotApplicable,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScoreUnitArg {
    Milliseconds,
    Score,
    OperationsPerSecond,
    Ratio,
    Percent,
}

impl From<ScoreUnitArg> for MetricUnit {
    fn from(value: ScoreUnitArg) -> Self {
        match value {
            ScoreUnitArg::Milliseconds => Self::Milliseconds,
            ScoreUnitArg::Score => Self::Score,
            ScoreUnitArg::OperationsPerSecond => Self::OperationsPerSecond,
            ScoreUnitArg::Ratio => Self::Ratio,
            ScoreUnitArg::Percent => Self::Percent,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScoreDirectionArg {
    LowerIsBetter,
    HigherIsBetter,
    Informational,
}

impl From<ScoreDirectionArg> for MetricDirection {
    fn from(value: ScoreDirectionArg) -> Self {
        match value {
            ScoreDirectionArg::LowerIsBetter => Self::LowerIsBetter,
            ScoreDirectionArg::HigherIsBetter => Self::HigherIsBetter,
            ScoreDirectionArg::Informational => Self::Informational,
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Run one command and emit an Otter benchmark JSON record")]
struct Args {
    #[arg(long, default_value = "external")]
    suite: String,
    #[arg(long)]
    name: String,
    /// Stable `key=value` workload parameter; repeat for multiple parameters.
    #[arg(long = "parameter")]
    parameters: Vec<String>,
    #[arg(long, value_enum, default_value = "external-process")]
    surface: ExecutionSurfaceArg,
    #[arg(long, value_enum, default_value = "not-applicable")]
    jit_policy: JitPolicyArg,
    #[arg(long)]
    jit_osr_threshold: Option<u32>,
    #[arg(long, value_enum, default_value = "not-applicable")]
    gc_policy: GcPolicyArg,
    #[arg(long)]
    gc_stress_stride: Option<u32>,
    #[arg(long, value_enum, default_value = "not-applicable")]
    runtime_reuse: RuntimeReuseArg,
    #[arg(long, value_enum, default_value = "not-applicable")]
    cache_state: CacheStateArg,
    #[arg(long)]
    validation_marker: Option<String>,
    /// Profile of the measured executable. Unknown unless explicitly asserted.
    #[arg(long)]
    build_profile: Option<String>,
    /// Enforced child-process wall timeout.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    timeout_ms: Option<u64>,
    /// Optional externally parsed suite score.
    #[arg(long)]
    score: Option<f64>,
    #[arg(long, default_value = "suite-score", requires = "score")]
    score_name: String,
    #[arg(long, value_enum, default_value = "score", requires = "score")]
    score_unit: ScoreUnitArg,
    #[arg(
        long,
        value_enum,
        default_value = "higher-is-better",
        requires = "score"
    )]
    score_direction: ScoreDirectionArg,
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
    timed_out: bool,
}

type OutputReceiver = Receiver<std::io::Result<Vec<u8>>>;

fn output_reader<R>(mut input: R) -> OutputReceiver
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let result = input.read_to_end(&mut output).map(|_| output);
        let _ = sender.send(result);
    });
    receiver
}

fn remaining_timeout(started: Instant, timeout_ms: u64) -> Option<Duration> {
    Duration::from_millis(timeout_ms).checked_sub(started.elapsed())
}

fn receive_output(
    receiver: &OutputReceiver,
    started: Instant,
    timeout_ms: Option<u64>,
) -> std::io::Result<Option<Vec<u8>>> {
    match timeout_ms {
        Some(timeout_ms) => {
            let Some(remaining) = remaining_timeout(started, timeout_ms) else {
                return Ok(None);
            };
            match receiver.recv_timeout(remaining) {
                Ok(output) => output.map(Some),
                Err(RecvTimeoutError::Timeout) => Ok(None),
                Err(RecvTimeoutError::Disconnected) => {
                    Err(std::io::Error::other("output reader disconnected"))
                }
            }
        }
        None => receiver
            .recv()
            .map_err(|_| std::io::Error::other("output reader disconnected"))?
            .map(Some),
    }
}

fn poll_interval_ms(timeout_ms: Option<u64>, rss_sample_ms: u64) -> u64 {
    match (timeout_ms, rss_sample_ms) {
        (Some(_), rss) if rss > 0 => rss.min(5),
        (Some(_), _) => 5,
        (None, rss) if rss > 0 => rss,
        (None, _) => 10,
    }
}

fn run_command(
    command: &[String],
    timeout_ms: Option<u64>,
    rss_sample_ms: u64,
) -> std::io::Result<RecordedOutput> {
    if timeout_ms.is_none() && rss_sample_ms == 0 {
        return Command::new(&command[0])
            .args(&command[1..])
            .output()
            .map(|output| RecordedOutput {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
                peak_rss_bytes: None,
                timed_out: false,
            });
    }

    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout_reader = output_reader(child.stdout.take().expect("piped stdout"));
    let stderr_reader = output_reader(child.stderr.take().expect("piped stderr"));

    #[cfg(feature = "rss")]
    let pid = sysinfo::Pid::from_u32(child.id());
    #[cfg(feature = "rss")]
    let mut system = sysinfo::System::new();
    #[cfg(feature = "rss")]
    let mut peak_rss_bytes = 0u64;
    #[cfg(not(feature = "rss"))]
    let peak_rss_bytes = 0u64;
    let started = Instant::now();
    let poll_ms = poll_interval_ms(timeout_ms, rss_sample_ms);
    #[cfg(feature = "rss")]
    let rss_interval = (rss_sample_ms > 0).then(|| Duration::from_millis(rss_sample_ms));
    #[cfg(feature = "rss")]
    let mut next_rss_sample = rss_interval.map(|_| started);
    let mut timed_out = false;
    let status = loop {
        #[cfg(feature = "rss")]
        if let (Some(interval), Some(next_sample)) = (rss_interval, next_rss_sample)
            && Instant::now() >= next_sample
        {
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
            if let Some(process) = system.process(pid) {
                peak_rss_bytes = peak_rss_bytes.max(process.memory());
            }
            next_rss_sample = Instant::now().checked_add(interval);
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if timeout_ms.is_some_and(|limit| started.elapsed() >= Duration::from_millis(limit)) {
            timed_out = true;
            if let Err(error) = child.kill() {
                if let Some(status) = child.try_wait()? {
                    timed_out = false;
                    break status;
                }
                return Err(error);
            }
            break child.wait()?;
        }
        let poll_interval = Duration::from_millis(poll_ms);
        let sleep_for = timeout_ms
            .and_then(|limit| remaining_timeout(started, limit))
            .map_or(poll_interval, |remaining| remaining.min(poll_interval));
        if !sleep_for.is_zero() {
            std::thread::sleep(sleep_for);
        }
    };

    #[cfg(feature = "rss")]
    if rss_sample_ms > 0 {
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if let Some(process) = system.process(pid) {
            peak_rss_bytes = peak_rss_bytes.max(process.memory());
        }
    }

    let stdout = if timed_out {
        Vec::new()
    } else {
        match receive_output(&stdout_reader, started, timeout_ms)? {
            Some(output) => output,
            None => {
                timed_out = true;
                Vec::new()
            }
        }
    };
    let stderr = if timed_out {
        Vec::new()
    } else {
        match receive_output(&stderr_reader, started, timeout_ms)? {
            Some(output) => output,
            None => {
                timed_out = true;
                Vec::new()
            }
        }
    };
    Ok(RecordedOutput {
        status,
        stdout,
        stderr,
        peak_rss_bytes: (peak_rss_bytes > 0).then_some(peak_rss_bytes),
        timed_out,
    })
}

fn parameters(values: Vec<String>) -> Result<BTreeMap<String, String>, String> {
    let mut parameters = BTreeMap::new();
    for value in values {
        let (key, value) = value
            .split_once('=')
            .ok_or_else(|| format!("parameter {value:?} must use key=value"))?;
        if key.is_empty() {
            return Err("parameter key must not be empty".into());
        }
        if parameters.insert(key.into(), value.into()).is_some() {
            return Err(format!("duplicate parameter {key:?}"));
        }
    }
    Ok(parameters)
}

const RECORDER_RSS_SAMPLE_MS_PARAMETER: &str = "recorder.rss-sample-ms";

fn benchmark_parameters(
    values: Vec<String>,
    rss_sample_ms: u64,
) -> Result<BTreeMap<String, String>, String> {
    let mut parameters = parameters(values)?;
    if parameters
        .insert(
            RECORDER_RSS_SAMPLE_MS_PARAMETER.into(),
            rss_sample_ms.to_string(),
        )
        .is_some()
    {
        return Err(format!(
            "parameter {RECORDER_RSS_SAMPLE_MS_PARAMETER:?} is reserved by the recorder"
        ));
    }
    Ok(parameters)
}

fn failure(kind: FailureKind, message: impl Into<String>) -> BenchmarkFailure {
    BenchmarkFailure {
        kind,
        message: message.into(),
    }
}

fn main() {
    let invocation = std::env::args().collect::<Vec<_>>();
    let args = Args::parse();
    #[cfg(feature = "rss")]
    let rss_sample_ms = args.rss_sample_ms;
    #[cfg(not(feature = "rss"))]
    let rss_sample_ms = 0;
    let parameter_result = benchmark_parameters(args.parameters, rss_sample_ms);
    let started = Instant::now();
    let output = match &parameter_result {
        Ok(_) => run_command(&args.command, args.timeout_ms, rss_sample_ms),
        Err(error) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            error.clone(),
        )),
    };
    let wall_time_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
    let peak_rss_bytes = output
        .as_ref()
        .ok()
        .and_then(|output| output.peak_rss_bytes);

    let mut metrics = Vec::new();
    let score_is_valid = args.score.is_none_or(f64::is_finite);
    if score_is_valid {
        metrics.push(
            Metric::from_u64_samples(
                "wall-time",
                MetricUnit::Nanoseconds,
                MetricDirection::LowerIsBetter,
                if args.score.is_some() {
                    MetricRole::Secondary
                } else {
                    MetricRole::Primary
                },
                vec![wall_time_ns],
                Statistic::Single,
            )
            .expect("one wall-time sample"),
        );
        if let Some(score) = args.score {
            metrics.push(
                Metric::from_decimal_samples(
                    args.score_name,
                    args.score_unit.into(),
                    args.score_direction.into(),
                    MetricRole::Primary,
                    vec![score],
                    Statistic::Single,
                )
                .expect("finite suite score"),
            );
        }
    }
    if let Some(peak_rss_bytes) = peak_rss_bytes {
        metrics.push(
            Metric::from_u64_samples(
                "peak-rss",
                MetricUnit::Bytes,
                MetricDirection::LowerIsBetter,
                MetricRole::Diagnostic,
                vec![peak_rss_bytes],
                Statistic::Single,
            )
            .expect("one RSS sample"),
        );
    }

    let (status, marker, exit_code, outcome_failure) = match output {
        Ok(output) if output.timed_out => (
            OutcomeStatus::TimedOut,
            None,
            output.status.code(),
            Some(failure(
                FailureKind::Timeout,
                format!("command exceeded {} ms", args.timeout_ms.unwrap()),
            )),
        ),
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let success = output.status.success();
            let marker_matches = args
                .validation_marker
                .as_ref()
                .is_some_and(|marker| stdout.contains(marker));
            if !success {
                (
                    OutcomeStatus::Failed,
                    None,
                    output.status.code(),
                    Some(failure(
                        FailureKind::Process,
                        String::from_utf8_lossy(&output.stderr).into_owned(),
                    )),
                )
            } else if !score_is_valid {
                (
                    OutcomeStatus::Failed,
                    None,
                    output.status.code(),
                    Some(failure(
                        FailureKind::Validation,
                        "suite score must be finite",
                    )),
                )
            } else if marker_matches {
                (
                    OutcomeStatus::Validated,
                    args.validation_marker.clone(),
                    output.status.code(),
                    None,
                )
            } else if let Some(marker) = &args.validation_marker {
                (
                    OutcomeStatus::Failed,
                    None,
                    output.status.code(),
                    Some(failure(
                        FailureKind::Validation,
                        format!("validation marker {marker:?} was not emitted"),
                    )),
                )
            } else {
                (OutcomeStatus::Unvalidated, None, output.status.code(), None)
            }
        }
        Err(error) => (
            OutcomeStatus::Failed,
            None,
            None,
            Some(failure(
                if parameter_result.is_err() {
                    FailureKind::Configuration
                } else {
                    FailureKind::Process
                },
                error.to_string(),
            )),
        ),
    };

    let result = BenchmarkResult {
        benchmark: BenchmarkIdentity {
            suite: args.suite,
            name: args.name,
            parameters: parameter_result.unwrap_or_default(),
        },
        provenance: BenchmarkProvenance::capture(
            args.build_profile.unwrap_or_else(|| "unknown".into()),
        ),
        configuration: BenchmarkConfiguration {
            surface: args.surface.into(),
            jit_policy: args.jit_policy.into(),
            jit_osr_threshold: args.jit_osr_threshold,
            gc_policy: args.gc_policy.into(),
            gc_stress_stride: args.gc_stress_stride,
            runtime_reuse: args.runtime_reuse.into(),
            cache_state: args.cache_state.into(),
        },
        sampling: SamplingPlan {
            warmup_count: 0,
            sample_count: 1,
            iterations_per_sample: None,
            timeout_ms: args.timeout_ms,
        },
        metrics,
        outcome: BenchmarkOutcome {
            status,
            validation_marker: marker,
            process_exit_code: exit_code,
            failure: outcome_failure,
        },
        command: invocation,
    };
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
    std::process::exit(i32::from(!result.is_scoreable()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_are_ordered_and_reject_duplicates() {
        let parsed = parameters(vec!["z=2".into(), "a=1".into()]).unwrap();
        assert_eq!(
            parsed.keys().cloned().collect::<Vec<_>>(),
            vec!["a".to_string(), "z".to_string()]
        );
        assert!(parameters(vec!["a=1".into(), "a=2".into()]).is_err());
        assert!(parameters(vec!["missing-separator".into()]).is_err());
    }

    #[test]
    fn current_tier_values_have_exact_meaning() {
        assert_eq!(
            JitPolicy::from(JitPolicyArg::Interpreter),
            JitPolicy::Interpreter
        );
        assert_eq!(JitPolicy::from(JitPolicyArg::Template), JitPolicy::Template);
        assert_eq!(
            JitPolicy::from(JitPolicyArg::ProductionTiered),
            JitPolicy::ProductionTiered
        );
    }

    #[test]
    fn clap_accepts_unscored_command_without_legacy_modes() {
        let args = Args::try_parse_from([
            "otter-benchmark",
            "--name",
            "smoke",
            "--validation-marker",
            "ok",
            "--",
            "true",
        ])
        .expect("minimal recorder command");
        assert_eq!(args.name, "smoke");
        assert!(args.score.is_none());
        assert!(args.build_profile.is_none());
        assert!(
            Args::try_parse_from([
                "otter-benchmark",
                "--name",
                "legacy",
                "--jit-mode",
                "baseline",
                "--",
                "true",
            ])
            .is_err()
        );
    }

    #[test]
    fn poll_interval_preserves_rss_cadence_without_a_timeout() {
        assert_eq!(poll_interval_ms(None, 250), 250);
        assert_eq!(poll_interval_ms(Some(1_000), 250), 5);
        assert_eq!(poll_interval_ms(Some(1_000), 2), 2);
    }

    #[test]
    fn recorder_rss_parameter_is_reserved() {
        let parsed = benchmark_parameters(vec!["workload=smoke".into()], 25).unwrap();
        assert_eq!(parsed[RECORDER_RSS_SAMPLE_MS_PARAMETER], "25");
        assert!(
            benchmark_parameters(vec!["recorder.rss-sample-ms=caller-controlled".into()], 25,)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_does_not_wait_for_descendant_held_pipes() {
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "sleep 2 & exit 0".to_owned(),
        ];
        let started = Instant::now();
        let output = run_command(&command, Some(50), 0).unwrap();
        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_millis(1_000));
    }
}
