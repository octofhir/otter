//! Otter foundation CLI: `otter` binary.
//!
//! Thin wrapper over [`otter_runtime`]. Implements the foundation-
//! phase command surface from
//! [the public runtime architecture](../../../docs/book/src/engine/architecture.md):
//! `run`, `<file>` shorthand, `eval`, `-e`, `-p`, `check`, `test`,
//! `install`, `add`, `remove`, `init`, `info`, `--dump-bytecode[=json]`. Slice tasks `09`+ extend
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

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use otter_bytecode::{disasm::disassemble, dump::to_json_pretty};
use otter_pm_manifest::{PACKAGE_JSON, PackageManifest, PackageType};
use otter_runtime::{
    BooleanPermission, CapabilitySet, Diagnostic, OtterError, Permission, SourceInput,
};
use otter_test::{Report, RunOptions, Suite};
use otter_web::WebApiBuilderExt;

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
    /// Write or verify the project `otter-lock` without executing lifecycle scripts.
    Install(InstallArgs),
    /// Add dependencies to `package.json`, then refresh `otter-lock`.
    Add(AddArgs),
    /// Remove dependencies from `package.json`, then refresh `otter-lock`.
    Remove(RemoveArgs),
    /// Create a new `package.json`.
    Init(InitArgs),
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
    /// File path, package script, or local package binary.
    target: String,
    /// Force package.json#scripts resolution.
    #[arg(long, conflicts_with = "bin")]
    script: bool,
    /// Force local package binary resolution.
    #[arg(long, conflicts_with = "script")]
    bin: bool,
    /// Forwarded target arguments.
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

#[derive(Debug, Args)]
struct InstallArgs {
    /// Project root.
    #[arg(long, default_value = ".")]
    root: PathBuf,
}

#[derive(Debug, Args)]
struct AddArgs {
    /// Project root.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Add to devDependencies.
    #[arg(long, conflicts_with_all = ["peer", "optional"])]
    dev: bool,
    /// Add to peerDependencies.
    #[arg(long, conflicts_with_all = ["dev", "optional"])]
    peer: bool,
    /// Add to optionalDependencies.
    #[arg(long, conflicts_with_all = ["dev", "peer"])]
    optional: bool,
    /// Package specs, for example `react@^19` or `@scope/pkg@1`.
    packages: Vec<String>,
}

#[derive(Debug, Args)]
struct RemoveArgs {
    /// Project root.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Package names to remove from all dependency buckets.
    packages: Vec<String>,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Project root.
    #[arg(default_value = ".")]
    root: PathBuf,
    /// Package name. Defaults to the directory name.
    #[arg(long)]
    name: Option<String>,
    /// Initial package version.
    #[arg(long, default_value = "0.1.0")]
    version: String,
    /// Accept defaults without prompting.
    #[arg(short = 'y', long)]
    yes: bool,
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
            run_target(args, json, dump_mode.as_deref(), &caps, &startup_timer).await
        }
        (Some(Command::Install(args)), _) => run_pm_install(&args.root, json).await,
        (Some(Command::Add(args)), _) => run_pm_add(args, json).await,
        (Some(Command::Remove(args)), _) => run_pm_remove(args, json).await,
        (Some(Command::Init(args)), _) => run_pm_init(args, json).await,
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
    let otter = cli_otter_builder(caps).build()?;
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunTarget {
    File(PathBuf),
    Script {
        project_root: PathBuf,
        name: String,
        command: String,
    },
    Bin(otter_pm::PackageBin),
}

async fn run_target(
    args: RunArgs,
    json: bool,
    dump_mode: Option<&str>,
    caps: &CapabilitySet,
    startup_timer: &CliStartupTimer,
) -> Result<ExitCode, OtterError> {
    let project_root = std::env::current_dir().map_err(|err| pm_config_error(err.to_string()))?;
    let target_args = args.args.clone();
    match resolve_run_target(&project_root, &args).await? {
        RunTarget::File(path) => run_file(&path, json, dump_mode, caps, startup_timer).await,
        RunTarget::Script {
            project_root,
            name: _,
            command,
        } => {
            if dump_mode.is_some() {
                return Err(pm_config_error(
                    "--dump-bytecode only supports file targets in this slice",
                ));
            }
            run_package_script(&project_root, &command, &target_args, json).await
        }
        RunTarget::Bin(bin) => run_file(&bin.path, json, dump_mode, caps, startup_timer).await,
    }
}

async fn resolve_run_target(project_root: &Path, args: &RunArgs) -> Result<RunTarget, OtterError> {
    if args.script {
        return resolve_run_script(project_root, &args.target).await;
    }
    if args.bin {
        return resolve_run_bin(project_root, &args.target).await;
    }
    if args.target.starts_with("http://") || args.target.starts_with("https://") {
        return Err(pm_config_error(
            "remote URL entrypoints are not supported in this slice",
        ));
    }

    if let Some(path) = explicit_file_target(&args.target).await? {
        return Ok(RunTarget::File(path));
    }

    let script = resolve_run_script(project_root, &args.target).await.ok();
    let bin = resolve_run_bin(project_root, &args.target).await.ok();
    match (script, bin) {
        (Some(_script), Some(bin)) => Err(pm_config_error(format!(
            "ambiguous run target `{}`\n  candidates:\n  - package script: package.json#scripts.{} (use `otter run --script {}`)\n  - local package binary: {} (use `otter run --bin {}`)",
            args.target,
            args.target,
            args.target,
            match &bin {
                RunTarget::Bin(bin) => bin.path.display().to_string(),
                _ => unreachable!("resolve_run_bin only returns Bin"),
            },
            args.target
        ))),
        (Some(script), None) => Ok(script),
        (None, Some(bin)) => Ok(bin),
        (None, None) => Ok(RunTarget::File(PathBuf::from(&args.target))),
    }
}

async fn explicit_file_target(target: &str) -> Result<Option<PathBuf>, OtterError> {
    if let Some(path) = target.strip_prefix("file://") {
        return Ok(Some(PathBuf::from(path)));
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(None);
    }
    let path = PathBuf::from(target);
    if path.is_absolute()
        || target.starts_with("./")
        || target.starts_with("../")
        || target.contains('/')
        || target.contains('\\')
    {
        Ok(Some(path))
    } else if tokio::fs::try_exists(&path)
        .await
        .map_err(|err| pm_io_error(&path, err))?
    {
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

async fn resolve_run_script(project_root: &Path, target: &str) -> Result<RunTarget, OtterError> {
    let manifest = PackageManifest::read_from_dir(project_root)
        .await
        .map_err(map_manifest_error)?;
    let command = manifest.scripts.get(target).cloned().ok_or_else(|| {
        pm_config_error(format!(
            "unknown package script `{target}`; available scripts: {}",
            candidate_list(manifest.scripts.keys())
        ))
    })?;
    Ok(RunTarget::Script {
        project_root: project_root.to_path_buf(),
        name: target.to_string(),
        command,
    })
}

async fn resolve_run_bin(project_root: &Path, target: &str) -> Result<RunTarget, OtterError> {
    let graph = otter_pm::resolve_local_project(project_root)
        .await
        .map_err(map_pm_error)?
        .graph;
    let bins = graph.resolve_bin(target);
    match bins {
        [] => Err(pm_config_error(format!(
            "unknown local package binary `{target}`"
        ))),
        [bin] => Ok(RunTarget::Bin(bin.clone())),
        many => Err(pm_config_error(format!(
            "ambiguous local package binary `{target}`\n  candidates:\n{}",
            many.iter()
                .map(|bin| format!("  - {} ({})", bin.path.display(), bin.package))
                .collect::<Vec<_>>()
                .join("\n")
        ))),
    }
}

async fn run_package_script(
    project_root: &Path,
    command: &str,
    args: &[String],
    json: bool,
) -> Result<ExitCode, OtterError> {
    let command = command_with_args(command, args);
    let status = shell_command(&command)
        .current_dir(project_root)
        .status()
        .await
        .map_err(|err| pm_config_error(format!("package script failed to start: {err}")))?;
    let code = status.code().unwrap_or(1).clamp(0, 255) as u8;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": status.success(),
                "exitCode": code
            })
        );
    }
    Ok(ExitCode::from(code))
}

fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut process = tokio::process::Command::new("cmd");
        process.arg("/C").arg(command);
        process
    }
    #[cfg(not(windows))]
    {
        let mut process = tokio::process::Command::new("sh");
        process.arg("-c").arg(command);
        process
    }
}

fn command_with_args(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        return command.to_string();
    }
    let mut out = command.to_string();
    for arg in args {
        out.push(' ');
        out.push_str(&shell_quote(arg));
    }
    out
}

fn shell_quote(value: &str) -> String {
    #[cfg(windows)]
    {
        format!("\"{}\"", value.replace('"', "\\\""))
    }
    #[cfg(not(windows))]
    {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn candidate_list<'a>(items: impl Iterator<Item = &'a String>) -> String {
    let items = items.cloned().collect::<Vec<_>>();
    if items.is_empty() {
        "<none>".to_string()
    } else {
        items.join(", ")
    }
}

async fn run_eval(
    source: &str,
    print: bool,
    json: bool,
    caps: &CapabilitySet,
    startup_timer: &CliStartupTimer,
) -> Result<ExitCode, OtterError> {
    let otter = cli_otter_builder(caps).build()?;
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

fn cli_otter_builder(caps: &CapabilitySet) -> otter_runtime::OtterBuilder {
    otter_runtime::Otter::builder()
        .capabilities(caps.clone())
        .with_web_apis()
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

async fn run_pm_init(args: InitArgs, json: bool) -> Result<ExitCode, OtterError> {
    tokio::fs::create_dir_all(&args.root)
        .await
        .map_err(|err| pm_io_error(&args.root, err))?;
    let manifest_path = args.root.join(PACKAGE_JSON);
    if tokio::fs::try_exists(&manifest_path)
        .await
        .map_err(|err| pm_io_error(&manifest_path, err))?
    {
        return Err(pm_config_error(format!(
            "{} already exists",
            manifest_path.display()
        )));
    }
    let manifest = build_init_manifest(&args)?;
    let name = manifest
        .name
        .clone()
        .unwrap_or_else(|| default_package_name(&args.root));
    manifest
        .write_to_dir(&args.root)
        .await
        .map_err(|err| pm_config_error(err.to_string()))?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "path": manifest_path,
                "name": name
            })
        );
    } else {
        println!("created {}", manifest_path.display());
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_pm_install(root: &Path, json: bool) -> Result<ExitCode, OtterError> {
    let changed = otter_pm::write_local_lockfile(root)
        .await
        .map_err(map_pm_error)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "lockfile": root.join(otter_pm_lockfile::LOCKFILE_NAME),
                "lockfileChanged": changed
            })
        );
    } else if changed {
        println!(
            "wrote {}",
            root.join(otter_pm_lockfile::LOCKFILE_NAME).display()
        );
    } else {
        println!(
            "{} is up to date",
            root.join(otter_pm_lockfile::LOCKFILE_NAME).display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_pm_add(args: AddArgs, json: bool) -> Result<ExitCode, OtterError> {
    if args.packages.is_empty() {
        return Err(pm_config_error("otter add requires at least one package"));
    }
    let mut manifest = PackageManifest::read_from_dir(&args.root)
        .await
        .map_err(map_manifest_error)?;
    let bucket = if args.dev {
        &mut manifest.dev_dependencies
    } else if args.peer {
        &mut manifest.peer_dependencies
    } else if args.optional {
        &mut manifest.optional_dependencies
    } else {
        &mut manifest.dependencies
    };
    let mut added = Vec::new();
    for spec in &args.packages {
        let (name, range) = parse_package_spec(spec);
        bucket.insert(name.clone(), range.clone());
        added.push(serde_json::json!({ "name": name, "range": range }));
    }
    manifest
        .write_to_dir(&args.root)
        .await
        .map_err(|err| pm_config_error(err.to_string()))?;
    let lockfile_changed = otter_pm::write_local_lockfile(&args.root)
        .await
        .map_err(map_pm_error)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "added": added,
                "lockfileChanged": lockfile_changed
            })
        );
    } else {
        for item in added {
            println!(
                "added {}@{}",
                item["name"].as_str().unwrap(),
                item["range"].as_str().unwrap()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_pm_remove(args: RemoveArgs, json: bool) -> Result<ExitCode, OtterError> {
    if args.packages.is_empty() {
        return Err(pm_config_error(
            "otter remove requires at least one package",
        ));
    }
    let mut manifest = PackageManifest::read_from_dir(&args.root)
        .await
        .map_err(map_manifest_error)?;
    let mut removed = 0usize;
    for package in &args.packages {
        removed += usize::from(manifest.dependencies.remove(package).is_some());
        removed += usize::from(manifest.dev_dependencies.remove(package).is_some());
        removed += usize::from(manifest.peer_dependencies.remove(package).is_some());
        removed += usize::from(manifest.optional_dependencies.remove(package).is_some());
    }
    manifest
        .write_to_dir(&args.root)
        .await
        .map_err(|err| pm_config_error(err.to_string()))?;
    let lockfile_changed = otter_pm::write_local_lockfile(&args.root)
        .await
        .map_err(map_pm_error)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "removed": removed,
                "lockfileChanged": lockfile_changed
            })
        );
    } else {
        println!(
            "removed {removed} dependency entr{}",
            if removed == 1 { "y" } else { "ies" }
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn build_init_manifest(args: &InitArgs) -> Result<PackageManifest, OtterError> {
    let default_name = args
        .name
        .clone()
        .unwrap_or_else(|| default_package_name(&args.root));
    if args.yes {
        return Ok(PackageManifest {
            name: Some(default_name),
            version: Some(args.version.clone()),
            package_type: Some(PackageType::Module),
            main: Some("index.ts".to_string()),
            ..PackageManifest::default()
        });
    }

    let name = prompt_with_default("package name", &default_name)?;
    let version = prompt_with_default("version", &args.version)?;
    let package_type = loop {
        let value = prompt_with_default("type (module/commonjs)", "module")?;
        match value.as_str() {
            "module" => break PackageType::Module,
            "commonjs" => break PackageType::CommonJs,
            other => eprintln!("invalid package type `{other}`; expected `module` or `commonjs`"),
        }
    };
    let main = prompt_with_default("entry point", "index.ts")?;
    let module = prompt_optional("module entry point")?;
    let test = prompt_optional("test script")?;

    let mut manifest = PackageManifest {
        name: Some(name),
        version: Some(version),
        package_type: Some(package_type),
        main: Some(main),
        module,
        ..PackageManifest::default()
    };
    if let Some(test) = test {
        manifest.scripts.insert("test".to_string(), test);
    }
    Ok(manifest)
}

fn prompt_with_default(label: &str, default: &str) -> Result<String, OtterError> {
    print!("{label} ({default}): ");
    io::stdout()
        .flush()
        .map_err(|err| pm_config_error(err.to_string()))?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|err| pm_config_error(err.to_string()))?;
    let value = input.trim();
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value.to_string())
    }
}

fn prompt_optional(label: &str) -> Result<Option<String>, OtterError> {
    print!("{label}: ");
    io::stdout()
        .flush()
        .map_err(|err| pm_config_error(err.to_string()))?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|err| pm_config_error(err.to_string()))?;
    let value = input.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.to_string()))
    }
}

fn parse_package_spec(spec: &str) -> (String, String) {
    let split = if spec.starts_with('@') {
        spec[1..].rfind('@').map(|index| index + 1)
    } else {
        spec.rfind('@').filter(|index| *index > 0)
    };
    match split {
        Some(index) => (spec[..index].to_string(), spec[index + 1..].to_string()),
        None => (spec.to_string(), "*".to_string()),
    }
}

fn default_package_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != ".")
        .unwrap_or("otter-app")
        .to_string()
}

fn map_manifest_error(err: otter_pm_manifest::ManifestError) -> OtterError {
    pm_config_error(err.to_string())
}

fn map_pm_error(err: otter_pm::PackageManagerError) -> OtterError {
    pm_config_error(err.to_string())
}

fn pm_io_error(path: &Path, err: std::io::Error) -> OtterError {
    pm_config_error(format!("I/O failed for `{}`: {err}", path.display()))
}

fn pm_config_error(message: impl Into<String>) -> OtterError {
    OtterError::Config {
        reason: otter_runtime::ConfigError::ConflictingCapabilities {
            message: message.into(),
        },
    }
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

    #[test]
    fn package_spec_parser_handles_scoped_ranges() {
        assert_eq!(
            parse_package_spec("@scope/pkg@^1.2.3"),
            ("@scope/pkg".to_string(), "^1.2.3".to_string())
        );
        assert_eq!(
            parse_package_spec("@scope/pkg"),
            ("@scope/pkg".to_string(), "*".to_string())
        );
        assert_eq!(
            parse_package_spec("react@^19"),
            ("react".to_string(), "^19".to_string())
        );
    }

    #[tokio::test]
    async fn pm_init_and_install_write_manifest_and_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        run_pm_init(
            InitArgs {
                root: tmp.path().to_path_buf(),
                name: Some("app".to_string()),
                version: "0.1.0".to_string(),
                yes: true,
            },
            true,
        )
        .await
        .unwrap();
        run_pm_add(
            AddArgs {
                root: tmp.path().to_path_buf(),
                dev: false,
                peer: false,
                optional: false,
                packages: vec!["left-pad@^1.3.0".to_string()],
            },
            true,
        )
        .await
        .unwrap();
        let manifest = tokio::fs::read_to_string(tmp.path().join(PACKAGE_JSON))
            .await
            .unwrap();
        assert!(manifest.contains("\"left-pad\": \"^1.3.0\""));
        let lockfile = tokio::fs::read_to_string(tmp.path().join(otter_pm_lockfile::LOCKFILE_NAME))
            .await
            .unwrap();
        assert!(lockfile.contains("left-pad@npm:^1.3.0"));
    }

    #[tokio::test]
    async fn run_target_resolves_existing_file_before_package_names() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("task.ts"), "undefined;")
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","scripts":{"task.ts":"echo script"}}"#,
        )
        .await
        .unwrap();
        let args = RunArgs {
            target: tmp.path().join("task.ts").to_string_lossy().to_string(),
            script: false,
            bin: false,
            args: Vec::new(),
        };
        assert!(matches!(
            resolve_run_target(tmp.path(), &args).await.unwrap(),
            RunTarget::File(_)
        ));
    }

    #[tokio::test]
    async fn run_target_reports_script_bin_ambiguity() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{
              "name":"app",
              "workspaces":["packages/*"],
              "scripts":{"tool":"echo script"}
            }"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("packages/tool"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("packages/tool/package.json"),
            r#"{"name":"tool","version":"1.0.0","bin":{"tool":"./tool.ts"}}"#,
        )
        .await
        .unwrap();
        let args = RunArgs {
            target: "tool".to_string(),
            script: false,
            bin: false,
            args: Vec::new(),
        };
        let err = resolve_run_target(tmp.path(), &args).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("ambiguous run target `tool`"));
        assert!(message.contains("otter run --script tool"));
        assert!(message.contains("otter run --bin tool"));
    }

    #[tokio::test]
    async fn run_target_force_bin_resolves_workspace_bin() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","workspaces":["packages/*"]}"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("packages/tool"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("packages/tool/package.json"),
            r#"{"name":"tool","version":"1.0.0","bin":"./tool.ts"}"#,
        )
        .await
        .unwrap();
        let args = RunArgs {
            target: "tool".to_string(),
            script: false,
            bin: true,
            args: Vec::new(),
        };
        let resolved = resolve_run_target(tmp.path(), &args).await.unwrap();
        match resolved {
            RunTarget::Bin(bin) => assert!(bin.path.ends_with("packages/tool/tool.ts")),
            other => panic!("expected bin, got {other:?}"),
        }
    }
}
