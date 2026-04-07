//! Test262 runner for the **new** Otter VM (`otter-runtime` / `otter-vm`).
//!
//! Phase 4 of the bootstrap plan. Reuses the existing metadata parser and
//! report infrastructure but executes tests through `OtterRuntime`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use colored::*;
use indicatif::{ProgressBar, ProgressStyle};
use walkdir::WalkDir;

use otter_runtime::{
    NativeFunctionDescriptor, OtterRuntime, RegisterValue, RunError, RuntimeState,
    VmNativeCallError,
};
use otter_test262::metadata::{ExecutionMode, TestMetadata};
use otter_test262::{RunSummary, TestOutcome, TestResult};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "test262")]
#[command(about = "Run Test262 conformance tests against the Otter VM")]
struct Cli {
    /// Path to test262 directory
    #[arg(short, long, default_value = "tests/test262")]
    test_dir: PathBuf,

    /// Filter tests by path pattern
    #[arg(short, long)]
    filter: Option<String>,

    /// Run only tests in this subdirectory (e.g., "built-ins/Math")
    #[arg(short = 'd', long)]
    subdir: Option<String>,

    /// Verbosity: -v colored dots, -vv names, -vvv output
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Maximum number of tests to run
    #[arg(short = 'n', long)]
    max_tests: Option<usize>,

    /// Timeout per test in milliseconds
    #[arg(long, default_value = "5000")]
    timeout: u64,

    /// Path for JSONL result log
    #[arg(long)]
    log: Option<PathBuf>,

    /// Features to skip (comma-separated)
    #[arg(long, value_delimiter = ',')]
    skip_features: Vec<String>,
}

// ---------------------------------------------------------------------------
// Shared harness state — thread-local for zero-overhead in fn-ptr callbacks
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct HarnessInner {
    print_output: Vec<String>,
    done_result: Option<Result<(), String>>,
}

thread_local! {
    static HARNESS_STATE: std::cell::RefCell<HarnessInner> =
        std::cell::RefCell::new(HarnessInner::default());
}

fn harness_clear() {
    HARNESS_STATE.with(|cell| {
        let mut inner = cell.borrow_mut();
        inner.print_output.clear();
        inner.done_result = None;
    });
}

// ---------------------------------------------------------------------------
// Native callbacks (fn pointers — work with VmNativeFunction ABI)
// ---------------------------------------------------------------------------

fn test262_print(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    HARNESS_STATE.with(|cell| {
        let mut inner = cell.borrow_mut();
        for arg in args {
            inner
                .print_output
                .push(format_register_value(*arg, runtime));
        }
    });
    Ok(RegisterValue::undefined())
}

fn test262_done(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    HARNESS_STATE.with(|cell| {
        let mut inner = cell.borrow_mut();
        if let Some(arg) = args.first()
            && *arg != RegisterValue::undefined()
            && *arg != RegisterValue::null()
            && arg.as_bool() != Some(false)
        {
            inner.done_result = Some(Err(format_register_value(*arg, runtime)));
            return;
        }
        inner.done_result = Some(Ok(()));
    });
    Ok(RegisterValue::undefined())
}

fn test262_realm_symbol(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let description = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .create_symbol_from_value(description)
        .map_err(|error| map_interpreter_error(error, runtime))
}

fn test262_realm_symbol_for(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let key = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .symbol_for_value(key)
        .map_err(|error| map_interpreter_error(error, runtime))
}

fn test262_realm_symbol_key_for(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    if !value.is_symbol() {
        let error = runtime
            .alloc_type_error("Symbol.keyFor requires a symbol argument")
            .map_err(|error| {
                VmNativeCallError::Internal(format!("TypeError allocation failed: {error}").into())
            })?;
        return Err(VmNativeCallError::Thrown(
            RegisterValue::from_object_handle(error.0),
        ));
    }
    let Some(key) = runtime.symbol_registry_key(value).map(str::to_owned) else {
        return Ok(RegisterValue::undefined());
    };
    let key = runtime.alloc_string(key);
    Ok(RegisterValue::from_object_handle(key.0))
}

fn install_realm_symbol_wrapper(
    runtime: &mut RuntimeState,
    realm_global: otter_runtime::ObjectHandle,
) -> Result<(), VmNativeCallError> {
    let symbol_property = runtime.intern_property_name("Symbol");
    let prototype_property = runtime.intern_property_name("prototype");
    let for_property = runtime.intern_property_name("for");
    let key_for_property = runtime.intern_property_name("keyFor");

    let wrapper_ctor = runtime.register_native_function(NativeFunctionDescriptor::method(
        "Symbol",
        0,
        test262_realm_symbol,
    ));
    let wrapper_ctor = runtime.alloc_host_function(wrapper_ctor);
    let symbol_prototype = runtime.intrinsics().symbol_prototype();
    runtime
        .objects_mut()
        .set_property(
            wrapper_ctor,
            prototype_property,
            RegisterValue::from_object_handle(symbol_prototype.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("realm Symbol.prototype install failed: {error:?}").into(),
            )
        })?;

    let wrapper_for = runtime.register_native_function(NativeFunctionDescriptor::method(
        "for",
        1,
        test262_realm_symbol_for,
    ));
    let wrapper_for = runtime.alloc_host_function(wrapper_for);
    runtime
        .objects_mut()
        .set_property(
            wrapper_ctor,
            for_property,
            RegisterValue::from_object_handle(wrapper_for.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("realm Symbol.for install failed: {error:?}").into(),
            )
        })?;

    let wrapper_key_for = runtime.register_native_function(NativeFunctionDescriptor::method(
        "keyFor",
        1,
        test262_realm_symbol_key_for,
    ));
    let wrapper_key_for = runtime.alloc_host_function(wrapper_key_for);
    runtime
        .objects_mut()
        .set_property(
            wrapper_ctor,
            key_for_property,
            RegisterValue::from_object_handle(wrapper_key_for.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("realm Symbol.keyFor install failed: {error:?}").into(),
            )
        })?;

    let symbols: Vec<_> = runtime.intrinsics().well_known_symbols().to_vec();
    for symbol in symbols {
        let property_name = symbol
            .description()
            .strip_prefix("Symbol.")
            .expect("well-known symbol descriptions use Symbol.<name>");
        let property = runtime.intern_property_name(property_name);
        let value = runtime.intrinsics().well_known_symbol_value(symbol);
        runtime
            .objects_mut()
            .set_property(wrapper_ctor, property, value)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("realm {} install failed: {error:?}", symbol.description()).into(),
                )
            })?;
    }

    runtime
        .objects_mut()
        .set_property(
            realm_global,
            symbol_property,
            RegisterValue::from_object_handle(wrapper_ctor.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("realm global Symbol install failed: {error:?}").into(),
            )
        })?;
    Ok(())
}

fn test262_create_realm(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let realm = runtime.alloc_object();
    let global = runtime.intrinsics().global_object();
    let realm_global = runtime.alloc_object_with_prototype(Some(global));
    install_realm_symbol_wrapper(runtime, realm_global)?;
    let global_property = runtime.intern_property_name("global");
    runtime
        .objects_mut()
        .set_property(
            realm,
            global_property,
            RegisterValue::from_object_handle(realm_global.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("$262.createRealm global install failed: {error:?}").into(),
            )
        })?;
    Ok(RegisterValue::from_object_handle(realm.0))
}

fn map_interpreter_error(
    error: otter_runtime::InterpreterError,
    runtime: &mut RuntimeState,
) -> VmNativeCallError {
    match error {
        otter_runtime::InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
        otter_runtime::InterpreterError::TypeError(message) => {
            let error = match runtime.alloc_type_error(&message) {
                Ok(error) => error,
                Err(error) => {
                    return VmNativeCallError::Internal(
                        format!("TypeError allocation failed: {error}").into(),
                    );
                }
            };
            VmNativeCallError::Thrown(RegisterValue::from_object_handle(error.0))
        }
        otter_runtime::InterpreterError::NativeCall(message) => {
            VmNativeCallError::Internal(message)
        }
        other => VmNativeCallError::Internal(format!("{other}").into()),
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

struct NewVmRunner {
    test_dir: PathBuf,
    filter: Option<String>,
    skip_features: HashSet<String>,
    harness_cache: HashMap<String, String>,
    timeout: Duration,
}

impl NewVmRunner {
    fn new(test_dir: &Path, timeout: Duration) -> Self {
        let harness_dir = test_dir.join("harness");
        let mut harness_cache = HashMap::new();

        if harness_dir.is_dir() {
            for entry in fs::read_dir(&harness_dir).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".js")
                    && let Ok(content) = fs::read_to_string(entry.path())
                {
                    harness_cache.insert(name, content);
                }
            }
        }

        Self {
            test_dir: test_dir.to_path_buf(),
            filter: None,
            skip_features: HashSet::new(),
            harness_cache,
            timeout,
        }
    }

    fn with_filter(mut self, filter: String) -> Self {
        self.filter = Some(filter);
        self
    }

    fn with_skip_features(mut self, features: Vec<String>) -> Self {
        self.skip_features = features.into_iter().collect();
        self
    }

    fn list_tests(&self, subdir: Option<&str>) -> Vec<PathBuf> {
        let base = match subdir {
            Some(sub) => self.test_dir.join("test").join(sub),
            None => self.test_dir.join("test"),
        };

        if !base.exists() {
            eprintln!("Test directory does not exist: {}", base.display());
            return Vec::new();
        }

        let mut tests: Vec<PathBuf> = WalkDir::new(&base)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension().is_some_and(|ext| ext == "js")
                    && !e.path().to_string_lossy().contains("_FIXTURE")
            })
            .map(|e| e.into_path())
            .collect();

        if let Some(ref filter) = self.filter {
            tests.retain(|p| p.to_string_lossy().contains(filter.as_str()));
        }

        tests.sort();
        tests
    }

    /// Install test262 harness globals on the runtime.
    fn setup_harness(rt: &mut OtterRuntime) {
        rt.state_mut()
            .install_native_global(NativeFunctionDescriptor::method("print", 1, test262_print));
        rt.state_mut()
            .install_native_global(NativeFunctionDescriptor::method("$DONE", 1, test262_done));

        let state = rt.state_mut();
        let create_realm = state.register_native_function(NativeFunctionDescriptor::method(
            "createRealm",
            0,
            test262_create_realm,
        ));
        let create_realm = state.alloc_host_function(create_realm);
        let test262 = state.alloc_object();
        let global = state.intrinsics().global_object();
        let global_property = state.intern_property_name("global");
        state
            .objects_mut()
            .set_property(
                test262,
                global_property,
                RegisterValue::from_object_handle(global.0),
            )
            .expect("$262.global should install");
        let create_realm_property = state.intern_property_name("createRealm");
        state
            .objects_mut()
            .set_property(
                test262,
                create_realm_property,
                RegisterValue::from_object_handle(create_realm.0),
            )
            .expect("$262.createRealm should install");
        state.install_global_value("$262", RegisterValue::from_object_handle(test262.0));
    }

    fn run_test_all_modes(&self, path: &Path) -> Vec<TestResult> {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                return vec![TestResult {
                    path: path.to_string_lossy().to_string(),
                    mode: ExecutionMode::NonStrict,
                    outcome: TestOutcome::Crash,
                    duration_ms: 0,
                    error: Some(format!("Failed to read file: {e}")),
                    features: vec![],
                }];
            }
        };

        let metadata = match TestMetadata::parse(&content) {
            Some(m) => m,
            None => {
                return vec![TestResult {
                    path: path.to_string_lossy().to_string(),
                    mode: ExecutionMode::NonStrict,
                    outcome: TestOutcome::Crash,
                    duration_ms: 0,
                    error: Some("Failed to parse test metadata".to_string()),
                    features: vec![],
                }];
            }
        };

        let relative_path = path.strip_prefix(&self.test_dir).unwrap_or(path);
        let relative_path_str = relative_path.to_string_lossy().replace('\\', "/");

        // Skip features.
        for feature in &metadata.features {
            if self.skip_features.contains(feature) {
                return vec![TestResult {
                    path: relative_path_str,
                    mode: ExecutionMode::NonStrict,
                    outcome: TestOutcome::Skip,
                    duration_ms: 0,
                    error: Some(format!("Skipped feature: {feature}")),
                    features: metadata.features.clone(),
                }];
            }
        }

        let modes = metadata.execution_modes();
        let mut results = Vec::with_capacity(modes.len());

        for mode in &modes {
            let start = Instant::now();
            let (outcome, error) =
                self.run_single_mode(&content, &metadata, *mode, &relative_path_str);
            results.push(TestResult {
                path: relative_path_str.clone(),
                mode: *mode,
                outcome,
                duration_ms: start.elapsed().as_millis() as u64,
                error,
                features: metadata.features.clone(),
            });
        }

        results
    }

    fn run_single_mode(
        &self,
        content: &str,
        metadata: &TestMetadata,
        mode: ExecutionMode,
        source_url: &str,
    ) -> (TestOutcome, Option<String>) {
        if mode == ExecutionMode::Module {
            return (
                TestOutcome::Skip,
                Some("Module mode not yet supported".to_string()),
            );
        }

        let test_body = content
            .find("---*/")
            .map(|i| &content[i + 5..])
            .unwrap_or(content);

        // Fresh runtime per test.
        harness_clear();
        let mut rt = OtterRuntime::builder().timeout(self.timeout).build();
        Self::setup_harness(&mut rt);

        if !metadata.is_raw() {
            let includes = self.harness_includes(metadata);
            for include in includes {
                let Some(harness_content) = self.harness_cache.get(&include) else {
                    return (
                        TestOutcome::Fail,
                        Some(format!("Harness file '{include}' not found in cache")),
                    );
                };
                let harness_source = Self::script_source_for_mode(harness_content, mode);
                let harness_url = format!("{source_url}::{include}");
                match rt.run_script(&harness_source, &harness_url) {
                    Ok(_) => {}
                    Err(RunError::Compile(e)) => {
                        return (
                            TestOutcome::Fail,
                            Some(format!("Harness compile error in {include}: {e}")),
                        );
                    }
                    Err(RunError::Runtime(e)) => {
                        return (
                            if e.contains("execution interrupted") {
                                TestOutcome::Timeout
                            } else {
                                TestOutcome::Fail
                            },
                            Some(format!("Harness runtime error in {include}: {e}")),
                        );
                    }
                }
            }
        }

        let test_source = Self::script_source_for_mode(test_body, mode);

        match rt.run_script(&test_source, source_url) {
            Ok(_) => {
                if metadata.expects_early_error() || metadata.expects_runtime_error() {
                    (
                        TestOutcome::Fail,
                        Some("Expected error but execution succeeded".to_string()),
                    )
                } else {
                    (TestOutcome::Pass, None)
                }
            }
            Err(RunError::Compile(e)) => {
                if metadata.expects_early_error() {
                    (TestOutcome::Pass, None)
                } else {
                    (TestOutcome::Fail, Some(format!("Compile error: {e}")))
                }
            }
            Err(RunError::Runtime(e)) => {
                if e.contains("execution interrupted") {
                    (TestOutcome::Timeout, Some(e))
                } else if metadata.expects_runtime_error() {
                    (TestOutcome::Pass, None)
                } else {
                    (TestOutcome::Fail, Some(format!("RuntimeError: {e}")))
                }
            }
        }
    }

    fn harness_includes(&self, metadata: &TestMetadata) -> Vec<String> {
        let mut includes = vec!["sta.js".to_string(), "assert.js".to_string()];
        if metadata.is_async() {
            includes.push("doneprintHandle.js".to_string());
        }
        for include in &metadata.includes {
            if !includes.contains(include) {
                includes.push(include.clone());
            }
        }
        includes
    }

    fn script_source_for_mode(source: &str, mode: ExecutionMode) -> String {
        if mode == ExecutionMode::Strict {
            let mut prefixed = String::with_capacity(source.len() + 16);
            prefixed.push_str("\"use strict\";\n");
            prefixed.push_str(source);
            prefixed
        } else {
            source.to_string()
        }
    }
}

fn format_register_value(value: RegisterValue, runtime: &mut RuntimeState) -> String {
    // ES spec §7.1.17 ToString — for Error objects this invokes
    // Error.prototype.toString (§20.5.3.4) which returns "Name: Message".
    runtime.js_to_string_infallible(value).to_string()
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    eprintln!("{}", "Otter Test262 Runner".bold().cyan());
    eprintln!("Test directory: {}", cli.test_dir.display());

    let mut runner = NewVmRunner::new(&cli.test_dir, Duration::from_millis(cli.timeout));

    if let Some(ref filter) = cli.filter {
        runner = runner.with_filter(filter.clone());
        eprintln!("Filter: {filter}");
    }
    if !cli.skip_features.is_empty() {
        eprintln!("Skipping features: {:?}", cli.skip_features);
        runner = runner.with_skip_features(cli.skip_features.clone());
    }

    let mut tests = runner.list_tests(cli.subdir.as_deref());

    if let Some(max) = cli.max_tests {
        tests.truncate(max);
    }

    let test_count = tests.len();
    eprintln!("Found {} test files", test_count);

    let show_progress = cli.verbose < 2;
    let pb = if show_progress {
        let pb = ProgressBar::new(test_count as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) | {msg}")
                .unwrap()
                .progress_chars("#>-"),
        );
        pb.set_message("starting...");
        Some(pb)
    } else {
        None
    };

    let mut log_writer: Option<std::io::BufWriter<fs::File>> = cli.log.as_ref().and_then(|p| {
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(p)
            .ok()
            .map(std::io::BufWriter::new)
    });

    let mut summary = RunSummary::new(10000);
    let run_start = Instant::now();

    for path in &tests {
        let results = runner.run_test_all_modes(path);

        for result in &results {
            match cli.verbose {
                0 => {}
                1 => {
                    let ch = match result.outcome {
                        TestOutcome::Pass => ".".green(),
                        TestOutcome::Fail => "F".red(),
                        TestOutcome::Skip => "S".yellow(),
                        TestOutcome::Timeout => "T".magenta(),
                        TestOutcome::Crash => "!".red().bold(),
                    };
                    eprint!("{ch}");
                    if summary.total % 80 == 79 {
                        eprintln!();
                    }
                }
                _ => {
                    let status = match result.outcome {
                        TestOutcome::Pass => "PASS".green(),
                        TestOutcome::Fail => "FAIL".red(),
                        TestOutcome::Skip => "SKIP".yellow(),
                        TestOutcome::Timeout => "TIME".magenta(),
                        TestOutcome::Crash => "CRASH".red().bold(),
                    };
                    eprintln!("[{status}] {} ({})", result.path, result.mode);
                    if let Some(ref err) = result.error {
                        eprintln!("  {err}");
                    }
                }
            }

            if let Some(ref mut writer) = log_writer
                && let Ok(line) = serde_json::to_string(result)
            {
                let _ = writeln!(writer, "{line}");
            }

            summary.record(result, false);
        }

        if let Some(ref pb) = pb {
            pb.inc(1);
            pb.set_message(format!(
                "Pass: {} Fail: {} Skip: {}",
                summary.passed, summary.failed, summary.skipped
            ));
        }
    }

    if let Some(ref mut writer) = log_writer {
        let _ = writer.flush();
    }

    if let Some(ref pb) = pb {
        pb.finish_and_clear();
    }
    if cli.verbose == 1 {
        eprintln!();
    }

    let duration = run_start.elapsed();
    let report = summary.into_report();
    report.print_summary();
    eprintln!("Duration: {:.1}s", duration.as_secs_f64());
}
