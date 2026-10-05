//! Prepare immutable seven-anchor Scripts and validate exact persistent records.
//!
//! # Contents
//! - `prepare`: all seven source/manifest/original-oracle files in a fresh directory.
//! - `prepare-startup`: all three immutable first-result sources and contracts.
//! - `freeze`: explicitly bind an untimed original semantic-result oracle.
//! - `validate`: owned observations and the existing exact-nanosecond Metric.
//!
//! # Invariants
//! - This tool parses/emits source; it never launches a JavaScript engine.
//! - Work cannot be reduced through CLI iteration arguments or case filtering.
//! - Input/output identities and phase validation precede any later scoring.
//!
//! # See also
//! - `otter_benchmark::warm_harness` owns the one current source/protocol contract.

use clap::{Parser, Subcommand};
use otter_benchmark::startup_harness::{StartupCase, prepare_startup_harness};
use otter_benchmark::warm_harness::{
    WarmAnchor, WarmHarnessManifest, WarmHarnessRequest, WarmInvocation, WarmSemanticResult,
    WarmSource, prepare_warm_harness, source_sha256, validate_warm_records,
};
use otter_benchmark::{Metric, SamplingPlan};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    PrepareStartup {
        #[arg(long)]
        fixtures: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Prepare {
        #[arg(long)]
        anchors: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 3)]
        warmups: u32,
        #[arg(long, default_value_t = 5)]
        samples: u32,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    Freeze {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        expected_result: PathBuf,
    },
    Validate {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        stdout: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AnchorInput {
    anchor: WarmAnchor,
    source: String,
    original_path: String,
    sha256: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedEntry {
    anchor: WarmAnchor,
    script: String,
    manifest: String,
    original_script: String,
}
#[derive(Serialize)]
struct PreparedIndex {
    anchors: Vec<PreparedEntry>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObservationOutput {
    anchor: WarmAnchor,
    measured_ns: Vec<u64>,
    warmups: Vec<WarmInvocation>,
    samples: Vec<WarmInvocation>,
    metric: Metric,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    std::fs::write(path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}
fn safe_filename(path: &str) -> bool {
    let mut components = Path::new(path).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn run(command: Command) -> Result<(), String> {
    match command {
        Command::PrepareStartup { fixtures, out } => {
            let mut prepared = Vec::new();
            for case in StartupCase::ALL {
                let (name, source) = match case {
                    StartupCase::EmptyCli => ("<empty-cli>".to_owned(), String::new()),
                    StartupCase::TinyJs | StartupCase::TinyTs => {
                        let path = fixtures.join(if case == StartupCase::TinyTs {
                            "tiny.ts"
                        } else {
                            "tiny.js"
                        });
                        (
                            path.display().to_string(),
                            std::fs::read_to_string(&path)
                                .map_err(|error| format!("{}: {error}", path.display()))?,
                        )
                    }
                };
                prepared.push(prepare_startup_harness(case, name, &source)?);
            }
            std::fs::create_dir(&out)
                .map_err(|error| format!("fresh output {}: {error}", out.display()))?;
            let mut manifests = Vec::new();
            for item in prepared {
                let case = item.manifest.case;
                std::fs::write(out.join(case.filename()), item.source)
                    .map_err(|error| error.to_string())?;
                write_json(
                    &out.join(format!("{}.manifest.json", case.name())),
                    &item.manifest,
                )?;
                manifests.push(item.manifest);
            }
            write_json(&out.join("startup.json"), &manifests)
        }
        Command::Prepare {
            anchors,
            out,
            warmups,
            samples,
            timeout_ms,
        } => {
            let inputs: Vec<AnchorInput> = read_json(&anchors)?;
            let names: BTreeSet<_> = inputs.iter().map(|input| input.anchor).collect();
            if inputs.len() != 7 || names != WarmAnchor::ALL.into_iter().collect::<BTreeSet<_>>() {
                return Err("prepare requires all seven unique original anchors".into());
            }
            let base = anchors.parent().unwrap_or_else(|| Path::new("."));
            let mut prepared = Vec::new();
            for input in inputs {
                if !safe_filename(&input.source)
                    || input.source != format!("{}.js", input.anchor.name())
                {
                    return Err("anchor source must be its canonical relative filename".into());
                }
                let path = base.join(&input.source);
                let source = std::fs::read_to_string(&path)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                let request = WarmHarnessRequest {
                    source: WarmSource {
                        anchor: input.anchor,
                        source_name: input.original_path,
                        original: source,
                        expected_sha256: input.sha256,
                    },
                    sampling: SamplingPlan {
                        warmup_count: warmups,
                        sample_count: samples,
                        iterations_per_sample: None,
                        timeout_ms,
                    },
                };
                prepared.push(prepare_warm_harness(request).map_err(|error| error.to_string())?);
            }
            std::fs::create_dir(&out)
                .map_err(|error| format!("fresh output {}: {error}", out.display()))?;
            let mut entries = Vec::new();
            for item in prepared {
                let anchor = item.manifest.anchor;
                let script = format!("{}.js", anchor.name());
                let manifest = format!("{}.manifest.json", anchor.name());
                let original_script = format!("{}.original.js", anchor.name());
                std::fs::write(out.join(&script), item.script)
                    .map_err(|error| error.to_string())?;
                std::fs::write(out.join(&original_script), item.original_script)
                    .map_err(|error| error.to_string())?;
                write_json(&out.join(&manifest), &item.manifest)?;
                entries.push(PreparedEntry {
                    anchor,
                    script,
                    manifest,
                    original_script,
                });
            }
            write_json(
                &out.join("prepared.json"),
                &PreparedIndex { anchors: entries },
            )
        }
        Command::Freeze {
            manifest,
            expected_result,
        } => {
            let mut data: WarmHarnessManifest = read_json(&manifest)?;
            let expected: WarmSemanticResult = read_json(&expected_result)?;
            if data.anchor != WarmAnchor::Crypto
                || data.expected_result.is_some()
                || !matches!(&expected,WarmSemanticResult::Text(text) if !text.is_empty())
            {
                return Err("freeze binds one nonempty untimed original crypto ciphertext to an unresolved manifest".into());
            }
            let directory = manifest.parent().unwrap_or_else(|| Path::new("."));
            for (filename, expected_hash) in [
                (format!("{}.js", data.anchor.name()), &data.generated_sha256),
                (
                    format!("{}.original.js", data.anchor.name()),
                    &data.original_script_sha256,
                ),
            ] {
                let bytes =
                    std::fs::read(directory.join(filename)).map_err(|error| error.to_string())?;
                if &source_sha256(&bytes) != expected_hash {
                    return Err("cannot freeze changed generated/original Script".into());
                }
            }
            data.expected_result = Some(expected);
            write_json(&manifest, &data)
        }
        Command::Validate {
            manifest,
            stdout,
            out,
        } => {
            let data: WarmHarnessManifest = read_json(&manifest)?;
            let bytes = std::fs::read(stdout).map_err(|error| error.to_string())?;
            let run = validate_warm_records(&data, &bytes).map_err(|error| error.to_string())?;
            let metric = run.metric()?;
            write_json(
                &out,
                &ObservationOutput {
                    anchor: run.anchor,
                    measured_ns: run.measured_ns,
                    warmups: run.warmups,
                    samples: run.samples,
                    metric,
                },
            )
        }
    }
}

fn main() -> ExitCode {
    match run(Args::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
