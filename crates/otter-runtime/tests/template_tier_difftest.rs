//! In-process differential gate for the template tier through the embedding API.
//!
//! # Contents
//! - Every committed difftest corpus program runs in both interpreter and template tiers.
//! - Completion, console output and errors are compared; progress identifies slow cases.
//!
//! # Invariants
//! - Production tier policy is unchanged; corpus loops must earn native admission.
//! - Each observation owns a fresh runtime. `otter-difftest` covers process isolation.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use otter_runtime::{ConsoleLevel, ConsoleSink, JitSelection, Otter};

#[derive(Debug, Default)]
struct LogCapture {
    events: Mutex<Vec<String>>,
}

impl LogCapture {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn snapshot(&self) -> Vec<String> {
        self.events.lock().expect("log mutex").clone()
    }
}

impl ConsoleSink for LogCapture {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        let line = format!("{level:?}: {}", fields.join(" "));
        self.events.lock().expect("log mutex").push(line);
    }
}

/// Full observable outcome of one corpus run under one tier selection.
#[derive(Debug, PartialEq, Eq)]
struct Observation {
    completion: Result<String, String>,
    console: Vec<String>,
}

fn run(source: &str, selection: JitSelection) -> Observation {
    let capture = LogCapture::new();
    let otter = Otter::builder()
        .console_sink(capture.clone())
        .jit_selection(selection)
        .build()
        .expect("otter build");
    let completion = otter
        .blocking_run_script(source)
        .map(|result| result.completion_string().to_owned())
        .map_err(|error| format!("{error:?}"));
    Observation {
        completion,
        console: capture.snapshot(),
    }
}

fn corpus_files() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../otter-difftest/corpus");
    let mut files: Vec<_> = std::fs::read_dir(&root)
        .unwrap_or_else(|error| panic!("read corpus {}: {error}", root.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "js"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "difftest corpus must not be empty");
    files
}

#[test]
fn template_tier_matches_the_interpreter_oracle_on_the_corpus() {
    let mut failures = Vec::new();
    for path in corpus_files() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let source = std::fs::read_to_string(&path).expect("read corpus source");
        eprintln!("CASE {name}: interpreter");
        let oracle = run(&source, JitSelection::InterpreterOnly);
        eprintln!("CASE {name}: template");
        let template = run(&source, JitSelection::Template);
        if template != oracle {
            failures.push(format!(
                "{name}: template diverged\n  oracle:   {oracle:?}\n  template: {template:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
