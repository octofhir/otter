//! Otter foundation CLI: `otter` binary.
//!
//! Thin wrapper over [`otter_runtime`]. Implements the foundation-
//! phase command surface from
//! [ADR-0003](../../../docs/new-engine/adr/0003-public-api-and-cli.md):
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
//!   the binary translates it to the documented exit code via
//!   `OtterError::exit_code` (ADR-0003 §4).
//! - JSON outputs (`--json`, `--dump-bytecode=json`, error payloads)
//!   match the wire formats locked by ADR-0003 and the bytecode-
//!   dump spec.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use otter_bytecode::{disasm::disassemble, dump::to_json_pretty};
use otter_runtime::{
    BooleanPermission, CapabilitySet, OtterError, Permission, Runtime, SourceInput,
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
    /// Build a [`CapabilitySet`] starting from `CapabilitySet::default()`
    /// (sensible defaults — see the type's docs) and apply CLI
    /// overrides on top.
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
    /// `-e <expr>` alias of `eval`.
    #[command(name = "-e", hide = true)]
    DashE(EvalArgs),
    /// `-p <expr>`: eval + print final value.
    #[command(name = "-p", hide = true)]
    DashP(EvalArgs),
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    let dump_mode = cli.dump_bytecode.clone();
    let caps = cli.perms.clone().into_capabilities();

    let result = match (cli.command, cli.args.first().cloned()) {
        // Explicit subcommand.
        (Some(Command::Run(args)), _) => run_file(&args.file, json, dump_mode.as_deref(), &caps),
        (Some(Command::Eval(args)), _) | (Some(Command::DashE(args)), _) => {
            run_eval(&args.expression, false, json, &caps)
        }
        (Some(Command::DashP(args)), _) => run_eval(&args.expression, true, json, &caps),
        (Some(Command::Check(args)), _) => run_check(&args.file, json, &caps),
        (Some(Command::Test(args)), _) => run_test(args, json),
        (Some(Command::Info), _) => run_info(json),
        // Shorthand: `otter <file>`.
        (None, Some(positional)) => run_file(
            &PathBuf::from(positional),
            json,
            dump_mode.as_deref(),
            &caps,
        ),
        (None, None) => {
            eprintln!("usage: otter <file> | otter <subcommand> [args...]");
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            emit_error(&err, json);
            ExitCode::from(u8::try_from(err.exit_code().clamp(0, 255)).unwrap_or(64))
        }
    }
}

fn build_runtime(caps: &CapabilitySet) -> Result<Runtime, OtterError> {
    Runtime::builder().capabilities(caps.clone()).build()
}

fn run_file(
    path: &std::path::Path,
    json: bool,
    dump_mode: Option<&str>,
    caps: &CapabilitySet,
) -> Result<ExitCode, OtterError> {
    if let Some(mode) = dump_mode {
        return run_dump(path, mode, caps);
    }
    // Route module-shaped files through the module-graph
    // pipeline; fall back to script execution otherwise. The
    // detection is AST-based (see `Otter::run_file` for the
    // shared helper used in the embedder Layer-A path).
    //
    // The `caps`-aware `Runtime` builder is constructed but not
    // used directly — module-mode runs go through `Otter::new()`
    // which uses the default capability set. Capability-respecting
    // module runs are a follow-up once `Runtime::run_module` lands
    // on the Layer-B builder.
    let _runtime = build_runtime(caps)?;
    let mut otter = otter_runtime::Otter::new();
    let result = otter.run_file(path)?;
    if json {
        println!(
            "{{\"completion\":{}}}",
            serde_json::to_string(&result.completion_string()).unwrap()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn run_eval(
    source: &str,
    print: bool,
    json: bool,
    caps: &CapabilitySet,
) -> Result<ExitCode, OtterError> {
    let mut runtime = build_runtime(caps)?;
    let result = runtime.eval(SourceInput::from_javascript(source))?;
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

fn run_check(
    path: &std::path::Path,
    json: bool,
    caps: &CapabilitySet,
) -> Result<ExitCode, OtterError> {
    let source = SourceInput::from_path(path)?;
    let specifier = path.to_string_lossy().to_string();
    let runtime = build_runtime(caps)?;
    runtime.check(source, &specifier)?;
    if json {
        println!("{{\"ok\":true}}");
    }
    Ok(ExitCode::SUCCESS)
}

fn run_dump(
    path: &std::path::Path,
    mode: &str,
    caps: &CapabilitySet,
) -> Result<ExitCode, OtterError> {
    let source = SourceInput::from_path(path)?;
    let specifier = path.to_string_lossy().to_string();
    let runtime = build_runtime(caps)?;
    let module = runtime.dump(source, &specifier)?;
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
    let info = serde_json::json!({
        "name": "otter",
        "version": env!("CARGO_PKG_VERSION"),
        "phase": "foundation",
        "interpreter_only": true,
        "edition": "2024",
    });
    if json {
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
