//! Otter foundation CLI: `otter` binary.
//!
//! Thin wrapper over [`otter_runtime`]. Implements the foundation-
//! phase command surface from
//! [the public runtime architecture](../../../docs/book/src/engine/architecture.md):
//! `run`, `<file>` shorthand, `eval`, `-e`, `-p`, `check`, `test`,
//! `info`, `--dump-bytecode[=json]`. Slice tasks `09`+ extend
//! behavior; this binary owns the argument parsing and exit-code
//! mapping.
//!
//! # Contents
//! - [`Cli`] — top-level [`clap`] derive struct.
//! - [`Command`] — explicit subcommands.
//! - [`main`] — argument parsing + dispatch.
//!
//! # Invariants
//! - Every fallible path returns [`otter_runtime::OtterError`] and
//!   the binary translates it through `OtterError::exit_code`.
//! - JSON outputs (`--json`, `--dump-bytecode=json`, error payloads)
//!   match the documented CLI and bytecode-dump wire formats.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use otter_bytecode::{disasm::disassemble, dump::to_json_pretty};
use otter_runtime::{
    BooleanPermission, CapabilitySet, Diagnostic, OtterError, Permission, SourceInput,
};
use otter_test::{Report, RunOptions, Suite};

/// Otter — JS/TS engine (foundation phase).
#[derive(Debug, Parser)]
#[command(name = "otter", version, about, long_about = None)]
struct Cli {
    /// Subcommand. When omitted and a positional argument is
    /// provided, the binary executes `otter run <file>` shorthand.
    #[command(subcommand)]
    command: Option<Command>,

    /// Evaluate JavaScript directly from the command line.
    #[arg(short = 'e', long = "eval", value_name = "code")]
    eval_source: Option<String>,

    /// Evaluate JavaScript and print the completion value.
    #[arg(short = 'p', long = "print", value_name = "code")]
    print_source: Option<String>,

    /// Shorthand: positional file path.
    #[arg(global = false, num_args = 0..)]
    args: Vec<String>,

    /// Print the bytecode disassembly for the script and exit.
    /// Use `--dump-bytecode` for text and `--dump-bytecode=json` for
    /// JSON; the file path is the next positional argument.
    #[arg(
        long = "dump-bytecode",
        value_name = "format",
        default_missing_value = "text",
        num_args = 0..=1,
        require_equals = true,
    )]
    dump_bytecode: Option<String>,

    /// Emit `--json` formatted output where applicable.
    #[arg(long, global = true)]
    json: bool,

    /// Capability flags (Deno-style).
    #[command(flatten)]
    perms: PermissionFlags,
}

/// Deno-style permission flags.
///
/// Each `--allow-*` flag accepts an optional comma-separated list of
/// patterns; passing the flag without a value enables the
/// permission unconditionally (`AllowAll`). Each `--deny-*` flag
/// always takes patterns (deny is a *narrowing* operation — Deno's
/// model). `--allow-all` short-circuits to grant everything; useful
/// for development.
#[derive(Debug, Clone, Default, Args)]
struct PermissionFlags {
    /// `--allow-read[=<paths>]` — read filesystem.
    #[arg(
        long = "allow-read",
        value_name = "paths",
        num_args = 0..=1,
        default_missing_value = "*",
        require_equals = true,
        global = true,
    )]
    allow_read: Option<String>,
    /// `--deny-read=<paths>` — explicitly deny these read paths.
    #[arg(long = "deny-read", value_name = "paths", global = true)]
    deny_read: Option<String>,

    /// `--allow-write[=<paths>]` — write filesystem.
    #[arg(
        long = "allow-write",
        value_name = "paths",
        num_args = 0..=1,
        default_missing_value = "*",
        require_equals = true,
        global = true,
    )]
    allow_write: Option<String>,
    /// `--deny-write=<paths>` — explicitly deny these write paths.
    #[arg(long = "deny-write", value_name = "paths", global = true)]
    deny_write: Option<String>,

    /// `--allow-net[=<hosts>]` — network access.
    #[arg(
        long = "allow-net",
        value_name = "hosts",
        num_args = 0..=1,
        default_missing_value = "*",
        require_equals = true,
        global = true,
    )]
    allow_net: Option<String>,
    /// `--deny-net=<hosts>` — explicitly deny these hosts.
    #[arg(long = "deny-net", value_name = "hosts", global = true)]
    deny_net: Option<String>,

    /// `--allow-env[=<vars>]` — environment variables.
    #[arg(
        long = "allow-env",
        value_name = "vars",
        num_args = 0..=1,
        default_missing_value = "*",
        require_equals = true,
        global = true,
    )]
    allow_env: Option<String>,
    /// `--deny-env=<vars>` — explicitly deny these env vars.
    #[arg(long = "deny-env", value_name = "vars", global = true)]
    deny_env: Option<String>,

    /// `--allow-run[=<commands>]` — subprocess.
    #[arg(
        long = "allow-run",
        value_name = "commands",
        num_args = 0..=1,
        default_missing_value = "*",
        require_equals = true,
        global = true,
    )]
    allow_run: Option<String>,
    /// `--deny-run=<commands>` — explicitly deny these commands.
    #[arg(long = "deny-run", value_name = "commands", global = true)]
    deny_run: Option<String>,

    /// `--allow-ffi[=<libs>]` — FFI loading.
    #[arg(
        long = "allow-ffi",
        value_name = "libs",
        num_args = 0..=1,
        default_missing_value = "*",
        require_equals = true,
        global = true,
    )]
    allow_ffi: Option<String>,
    /// `--deny-ffi=<libs>` — explicitly deny these libs.
    #[arg(long = "deny-ffi", value_name = "libs", global = true)]
    deny_ffi: Option<String>,

    /// `--allow-hrtime` — high-resolution time.
    #[arg(long, global = true)]
    allow_hrtime: bool,

    /// `--allow-all` — grant every capability unconditionally
    /// (development only).
    #[arg(long = "allow-all", global = true)]
    allow_all: bool,

    /// `--sandbox` — deny every capability. Use when running
    /// untrusted code.
    #[arg(long, global = true, conflicts_with = "allow_all")]
    sandbox: bool,
}

impl PermissionFlags {
    /// Build a deny-by-default [`CapabilitySet`] and apply CLI overrides on top.
    fn into_capabilities(self) -> CapabilitySet {
        if self.allow_all {
            return CapabilitySet::allow_all();
        }
        if self.sandbox {
            return CapabilitySet::sandbox();
        }
        let mut caps = CapabilitySet::default();
        apply_path_override(
            &mut caps.read,
            self.allow_read.as_deref(),
            self.deny_read.as_deref(),
        );
        apply_path_override(
            &mut caps.write,
            self.allow_write.as_deref(),
            self.deny_write.as_deref(),
        );
        apply_string_override(
            &mut caps.net,
            self.allow_net.as_deref(),
            self.deny_net.as_deref(),
        );
        apply_string_override(
            &mut caps.env,
            self.allow_env.as_deref(),
            self.deny_env.as_deref(),
        );
        apply_string_override(
            &mut caps.run,
            self.allow_run.as_deref(),
            self.deny_run.as_deref(),
        );
        apply_path_override(
            &mut caps.ffi,
            self.allow_ffi.as_deref(),
            self.deny_ffi.as_deref(),
        );
        if self.allow_hrtime {
            caps.hrtime = BooleanPermission::Allow;
        }
        caps
    }
}

fn apply_path_override(slot: &mut Permission<PathBuf>, allow: Option<&str>, deny: Option<&str>) {
    if allow.is_some() || deny.is_some() {
        *slot = build_path_perm(allow, deny);
    }
}

fn apply_string_override(slot: &mut Permission<String>, allow: Option<&str>, deny: Option<&str>) {
    if allow.is_some() || deny.is_some() {
        *slot = build_string_perm(allow, deny);
    }
}

fn build_path_perm(allow: Option<&str>, deny: Option<&str>) -> Permission<PathBuf> {
    match (allow, deny) {
        (None, None) => Permission::Deny,
        (Some("*"), deny) => match deny {
            None => Permission::AllowAll,
            Some(d) => Permission::Scoped {
                allow_list: Vec::new(),
                deny_list: parse_paths(d),
            },
        },
        (Some(allow_list), deny) => Permission::Scoped {
            allow_list: parse_paths(allow_list),
            deny_list: deny.map(parse_paths).unwrap_or_default(),
        },
        (None, Some(deny_list)) => Permission::Scoped {
            allow_list: Vec::new(),
            deny_list: parse_paths(deny_list),
        },
    }
}

fn build_string_perm(allow: Option<&str>, deny: Option<&str>) -> Permission<String> {
    match (allow, deny) {
        (None, None) => Permission::Deny,
        (Some("*"), deny) => match deny {
            None => Permission::AllowAll,
            Some(d) => Permission::Scoped {
                allow_list: Vec::new(),
                deny_list: parse_strings(d),
            },
        },
        (Some(allow_list), deny) => Permission::Scoped {
            allow_list: parse_strings(allow_list),
            deny_list: deny.map(parse_strings).unwrap_or_default(),
        },
        (None, Some(deny_list)) => Permission::Scoped {
            allow_list: Vec::new(),
            deny_list: parse_strings(deny_list),
        },
    }
}

fn parse_paths(s: &str) -> Vec<PathBuf> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn parse_strings(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a script file.
    Run(RunArgs),
    /// Evaluate an expression.
    Eval(EvalArgs),
    /// Compile / type-check without executing.
    Check(CheckArgs),
    /// Run the engine test harness.
    Test(TestArgs),
    /// Print build/runtime feature flags.
    Info,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Script path (`.js`, `.mjs`, `.cjs`, `.ts`, `.mts`, `.cts`).
    file: PathBuf,
    /// Forwarded script arguments (recorded only; unused this slice).
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

#[derive(Debug, Args)]
struct EvalArgs {
    /// Print the completion value, like `node -p` / `deno eval -p`.
    #[arg(short = 'p', long = "print")]
    print: bool,

    /// Expression / statement to evaluate.
    expression: String,
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// Source file to check.
    file: PathBuf,
}

#[derive(Debug, Args)]
struct TestArgs {
    /// Suite name.
    #[arg(long, default_value = "engine")]
    suite: String,
    /// Substring filter on fixture path / declared name.
    #[arg(long)]
    filter: Option<String>,
    /// Override the suite root.
    #[arg(long)]
    root: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let startup_timer = CliStartupTimer::from_env();
    let cli = Cli::parse();
    startup_timer.mark("parse_args");
    let json = cli.json;
    let dump_mode = cli.dump_bytecode.clone();
    let caps = cli.perms.clone().into_capabilities();
    startup_timer.mark("build_capabilities");
    let inline_eval = cli.eval_source.clone();
    let inline_print = cli.print_source.clone();

    if inline_eval.is_some() && inline_print.is_some() {
        eprintln!("error: --eval and --print cannot be used together");
        return ExitCode::from(2);
    }

    if inline_eval.is_some() || inline_print.is_some() {
        if cli.command.is_some() || !cli.args.is_empty() || dump_mode.is_some() {
            eprintln!("error: inline eval/print cannot be combined with a subcommand or file path");
            return ExitCode::from(2);
        }
        let (source, print) = match (inline_eval.as_deref(), inline_print.as_deref()) {
            (Some(source), None) => (source, false),
            (None, Some(source)) => (source, true),
            _ => unreachable!("checked conflicting inline eval modes above"),
        };
        let result = run_eval(source, print, json, &caps, &startup_timer).await;
        startup_timer.finish();
        return exit_from_result(result, json);
    }

    let result = match (cli.command, cli.args.first().cloned()) {
        // Explicit subcommand.
        (Some(Command::Run(args)), _) => {
            run_file(
                &args.file,
                json,
                dump_mode.as_deref(),
                &caps,
                &startup_timer,
            )
            .await
        }
        (Some(Command::Eval(args)), _) => {
            run_eval(&args.expression, args.print, json, &caps, &startup_timer).await
        }
        (Some(Command::Check(args)), _) => run_check(&args.file, json),
        (Some(Command::Test(args)), _) => run_test(args, json),
        (Some(Command::Info), _) => run_info(json),
        // Shorthand: `otter <file>`.
        (None, Some(positional)) => {
            run_file(
                &PathBuf::from(positional),
                json,
                dump_mode.as_deref(),
                &caps,
                &startup_timer,
            )
            .await
        }
        (None, None) => {
            eprintln!("usage: otter <file> | otter <subcommand> [args...]");
            return ExitCode::from(2);
        }
    };

    startup_timer.finish();
    exit_from_result(result, json)
}

struct CliStartupTimer {
    enabled: bool,
    start: Instant,
}

impl CliStartupTimer {
    fn from_env() -> Self {
        Self {
            enabled: std::env::var_os("OTTER_CLI_STARTUP_TIMINGS").is_some(),
            start: Instant::now(),
        }
    }

    fn mark(&self, label: &str) {
        if self.enabled {
            eprintln!(
                "otter_cli_startup phase={label} elapsed_us={}",
                self.start.elapsed().as_micros()
            );
        }
    }

    fn finish(&self) {
        self.mark("done");
    }
}

fn exit_from_result(result: Result<ExitCode, OtterError>, json: bool) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(err) => {
            emit_error(&err, json);
            ExitCode::from(u8::try_from(err.exit_code().clamp(0, 255)).unwrap_or(64))
        }
    }
}

async fn run_file(
    path: &std::path::Path,
    json: bool,
    dump_mode: Option<&str>,
    caps: &CapabilitySet,
    startup_timer: &CliStartupTimer,
) -> Result<ExitCode, OtterError> {
    if let Some(mode) = dump_mode {
        return run_dump(path, mode);
    }
    // Route module-shaped files through the module-graph
    // pipeline; fall back to script execution otherwise. The
    // detection is AST-based (see `Otter::run_file` for the
    // shared helper used in the embedder Layer-A path).
    //
    let otter = otter_runtime::Otter::builder()
        .capabilities(caps.clone())
        .build()?;
    startup_timer.mark("runtime_build");
    let result = otter.run_file(path).await?;
    startup_timer.mark("runtime_run_file");
    if json {
        println!(
            "{{\"completion\":{}}}",
            serde_json::to_string(&result.completion_string()).unwrap()
        );
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_eval(
    source: &str,
    print: bool,
    json: bool,
    caps: &CapabilitySet,
    startup_timer: &CliStartupTimer,
) -> Result<ExitCode, OtterError> {
    let otter = otter_runtime::Otter::builder()
        .capabilities(caps.clone())
        .build()?;
    startup_timer.mark("runtime_build");
    let result = otter.eval(source).await?;
    startup_timer.mark("runtime_eval");
    if print {
        println!("{}", result.completion_string());
    } else if json {
        println!(
            "{{\"completion\":{}}}",
            serde_json::to_string(&result.completion_string()).unwrap()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn run_check(path: &std::path::Path, json: bool) -> Result<ExitCode, OtterError> {
    compile_source_for_cli(path)?;
    if json {
        println!("{{\"ok\":true}}");
    }
    Ok(ExitCode::SUCCESS)
}

fn run_dump(path: &std::path::Path, mode: &str) -> Result<ExitCode, OtterError> {
    let module = compile_source_for_cli(path)?;
    let text = match mode {
        "json" => to_json_pretty(&module).map_err(|e| OtterError::Internal {
            code: "DUMP_JSON".to_string(),
            message: e.to_string(),
        })?,
        _ => disassemble(&module),
    };
    print!("{text}");
    Ok(ExitCode::SUCCESS)
}

fn compile_source_for_cli(
    path: &std::path::Path,
) -> Result<otter_bytecode::BytecodeModule, OtterError> {
    let source = SourceInput::from_path(path)?;
    let specifier = path.to_string_lossy().to_string();
    otter_compiler::compile_source(&source.text, source.kind, &specifier).map_err(map_compile_error)
}

fn map_compile_error(err: otter_compiler::CompileError) -> OtterError {
    use otter_compiler::CompileError;
    match err {
        CompileError::Syntax { messages } => OtterError::Compile {
            diagnostics: vec![Diagnostic::syntax(messages.join("; "))],
        },
        CompileError::Unsupported { node, span } => OtterError::Compile {
            diagnostics: vec![Diagnostic::unsupported(
                format!("unsupported AST node: {node}"),
                span,
            )],
        },
        CompileError::TypeScriptUnsupported { node, span } => OtterError::Compile {
            diagnostics: vec![Diagnostic::ts_unsupported(
                format!("typescript {node} is not supported in foundation"),
                span,
            )],
        },
        _ => OtterError::Internal {
            code: "COMPILE_UNKNOWN".to_string(),
            message: "unknown compiler error variant".to_string(),
        },
    }
}

fn run_test(args: TestArgs, json: bool) -> Result<ExitCode, OtterError> {
    let suite = match args.suite.as_str() {
        "engine" => Suite::Engine,
        "smoke" => Suite::Smoke,
        "test262" => Suite::Test262,
        other => {
            return Err(OtterError::Config {
                reason: otter_runtime::ConfigError::ConflictingCapabilities {
                    message: format!("unknown suite: {other}"),
                },
            });
        }
    };
    let opts = RunOptions {
        suite,
        filter: args.filter,
        root_override: args.root,
    };
    let report = otter_test::run_suite(&opts)?;
    print_test_report(&report, json);
    if report.all_passed() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

fn run_info(json: bool) -> Result<ExitCode, OtterError> {
    if json {
        let info = serde_json::json!({
            "name": "otter",
            "version": env!("CARGO_PKG_VERSION"),
            "phase": "foundation",
            "interpreter_only": true,
            "rust_edition": "2024",
        });
        println!("{}", serde_json::to_string(&info).unwrap());
    } else {
        println!(
            "otter v{} (foundation, interpreter-only)",
            env!("CARGO_PKG_VERSION")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn print_test_report(report: &Report, json: bool) {
    if json {
        for record in &report.records {
            println!("{}", serde_json::to_string(record).unwrap());
        }
        println!("{}", serde_json::to_string(&report.summary).unwrap());
    } else {
        for r in &report.records {
            println!(
                "{:<10} {:>5}ms  {}",
                outcome_label(&r.outcome),
                r.duration_ms,
                r.name
            );
        }
        let s = &report.summary;
        println!(
            "passed: {}  failed: {}  timeout: {}  oom: {}  cap: {}  skipped: {}  crash: {}  ({}ms)",
            s.passed,
            s.failed,
            s.timeout,
            s.oom,
            s.capability_denied,
            s.skipped,
            s.crash,
            s.duration_ms
        );
    }
}

fn outcome_label(outcome: &otter_test::Outcome) -> &'static str {
    use otter_test::Outcome::*;
    match outcome {
        Passed => "PASS",
        Failed { .. } => "FAIL",
        Timeout => "TIME",
        OutOfMemory => "OOM",
        CapabilityDenied { .. } => "CAP",
        Skipped { .. } => "SKIP",
        Crash { .. } => "CRASH",
    }
}

fn emit_error(err: &OtterError, json: bool) {
    if json {
        match err.to_json() {
            Ok(s) => eprintln!("{s}"),
            Err(_) => eprintln!("error: {err}"),
        }
    } else {
        eprintln!("error: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_eval_flag_parses_node_style() {
        let cli = Cli::try_parse_from(["otter", "-e", "42"]).expect("parse -e");
        assert_eq!(cli.eval_source.as_deref(), Some("42"));
        assert!(cli.print_source.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn top_level_print_flag_parses_node_style() {
        let cli = Cli::try_parse_from(["otter", "--print", "40 + 2"]).expect("parse --print");
        assert_eq!(cli.print_source.as_deref(), Some("40 + 2"));
        assert!(cli.eval_source.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn eval_subcommand_print_flag_parses_deno_style() {
        let cli = Cli::try_parse_from(["otter", "eval", "-p", "40 + 2"]).expect("parse eval -p");
        match cli.command {
            Some(Command::Eval(args)) => {
                assert!(args.print);
                assert_eq!(args.expression, "40 + 2");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
