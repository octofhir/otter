//! Otter foundation CLI: `otter` binary.
//!
//! Thin wrapper over [`otter_runtime`]. Implements the foundation-
//! phase command surface from
//! [the public runtime architecture](../../../docs/book/src/engine/architecture.md):
//! `run`, `<file>` shorthand, `eval`, `-e`, `-p`, `check`, `test`,
//! `install`, `add`, `remove`, `outdated`, `init`, `info`, `--dump-bytecode[=json]`. Slice tasks `09`+ extend
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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use otter_bytecode::disasm::disassemble;
use otter_node::NodeApiBuilderExt;
use otter_pm_lockfile::Lockfile;
use otter_pm_manifest::{PACKAGE_JSON, PackageBinManifest, PackageManifest, PackageType};
use otter_runtime::{CapabilitySet, DiagnosticCode, OtterError, Permission};
use otter_web::WebApiBuilderExt;
use semver::{Version, VersionReq};

mod error_render;

use error_render::emit_error;

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

    /// Emit a per-instruction step trace to stderr (or to the path
    /// given with `--trace=<path>`). Off when omitted.
    #[arg(
        long = "trace",
        value_name = "path",
        num_args = 0..=1,
        default_missing_value = "-",
        require_equals = true,
        global = true,
    )]
    trace: Option<String>,

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
    /// Write or verify the project `otter.lock` without executing lifecycle scripts.
    Install(InstallArgs),
    /// Add dependencies to `package.json`, then refresh `otter.lock`.
    Add(AddArgs),
    /// Remove dependencies from `package.json`, then refresh `otter.lock`.
    Remove(RemoveArgs),
    /// Check registry versions newer than the installed lockfile.
    Outdated(OutdatedArgs),
    /// Create a new `package.json`.
    Init(InitArgs),
    /// Evaluate an expression.
    Eval(EvalArgs),
    /// Compile / type-check without executing.
    Check(CheckArgs),
    /// Run tests with the hosted `node:test` runner.
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
    /// Emit VM stack CPU profile artifacts after the run.
    #[arg(long)]
    cpu_prof: bool,
    /// Directory for `--cpu-prof` artifacts.
    #[arg(long, default_value = "/tmp/otter-prof", requires = "cpu_prof")]
    cpu_prof_dir: PathBuf,
    /// Sample every N bytecode dispatch ticks for `--cpu-prof`.
    #[arg(long, default_value_t = 1000, requires = "cpu_prof")]
    cpu_prof_interval: u64,
    /// Base file name for `--cpu-prof` artifacts.
    #[arg(long, requires = "cpu_prof")]
    cpu_prof_name: Option<String>,
    /// GC heap cap in bytes; `0` disables the cap. Default is the runtime's
    /// built-in limit. Surfaces a catchable `RangeError` when exceeded.
    #[arg(long)]
    max_heap_bytes: Option<u64>,
    /// Forwarded target arguments.
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

#[derive(Debug, Clone)]
struct CpuProfileOptions {
    dir: PathBuf,
    interval: u64,
    name: Option<String>,
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
struct OutdatedArgs {
    /// Project root.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Include devDependencies.
    #[arg(long)]
    dev: bool,
    /// Include peerDependencies.
    #[arg(long)]
    peer: bool,
    /// Include optionalDependencies.
    #[arg(long)]
    optional: bool,
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
    /// Test files or directories. When omitted, discovers tests under `test/`.
    paths: Vec<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let startup_timer = CliStartupTimer::from_env();
    let cli = Cli::parse();
    startup_timer.mark("parse_args");
    install_cli_trace_target(cli.trace.clone());
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
        (Some(Command::Outdated(args)), _) => run_pm_outdated(args, json).await,
        (Some(Command::Init(args)), _) => run_pm_init(args, json).await,
        (Some(Command::Eval(args)), _) => {
            run_eval(&args.expression, args.print, json, &caps, &startup_timer).await
        }
        (Some(Command::Check(args)), _) => run_check(&args.file, json, &caps).await,
        (Some(Command::Test(args)), _) => run_node_tests(args, json, &caps, &startup_timer).await,
        (Some(Command::Info), _) => run_info(json),
        // Shorthand: `otter <file> [args...]`, routed through
        // the same resolver/session path as `otter run`.
        (None, Some(positional)) => {
            let forwarded_args = cli.args.iter().skip(1).cloned().collect::<Vec<_>>();
            run_target(
                RunArgs {
                    target: positional,
                    script: false,
                    bin: false,
                    cpu_prof: false,
                    cpu_prof_dir: PathBuf::from("/tmp/otter-prof"),
                    cpu_prof_interval: 1000,
                    cpu_prof_name: None,
                    max_heap_bytes: None,
                    args: forwarded_args,
                },
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

#[allow(clippy::too_many_arguments)]
async fn run_file(
    path: &std::path::Path,
    args: &[String],
    json: bool,
    dump_mode: Option<&str>,
    caps: &CapabilitySet,
    startup_timer: &CliStartupTimer,
    cpu_profile: Option<&CpuProfileOptions>,
    max_heap_bytes: Option<u64>,
) -> Result<ExitCode, OtterError> {
    run_file_with_cwd(
        path,
        args,
        None,
        json,
        dump_mode,
        caps,
        startup_timer,
        cpu_profile,
        max_heap_bytes,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_file_with_cwd(
    path: &std::path::Path,
    args: &[String],
    process_cwd: Option<&Path>,
    json: bool,
    dump_mode: Option<&str>,
    caps: &CapabilitySet,
    startup_timer: &CliStartupTimer,
    cpu_profile: Option<&CpuProfileOptions>,
    max_heap_bytes: Option<u64>,
) -> Result<ExitCode, OtterError> {
    if let Some(mode) = dump_mode {
        if cpu_profile.is_some() {
            return Err(pm_config_error(
                "--cpu-prof cannot be combined with --dump-bytecode",
            ));
        }
        return run_dump(path, mode, caps, startup_timer).await;
    }
    // Route module-shaped files through the module-graph
    // pipeline; fall back to script execution otherwise. The
    // detection is AST-based (see `Otter::run_file` for the
    // shared helper used in the embedder Layer-A path).
    //
    if let Some(profile) = cpu_profile {
        return run_file_with_cpu_profile(
            path,
            args,
            process_cwd,
            json,
            caps,
            startup_timer,
            profile,
            max_heap_bytes,
        )
        .await;
    }
    let mut builder = cli_otter_builder(caps)
        .process_argv(process_argv_for_file(path, args))
        .module_loader(cli_loader_config_for_entry(path).await);
    if let Some(bytes) = max_heap_bytes {
        builder = builder.max_heap_bytes(bytes);
    }
    if let Some(cwd) = process_cwd {
        builder = builder.process_cwd(cwd.to_path_buf());
    }
    let otter = builder.build()?;
    startup_timer.mark("runtime_build");
    let result = otter.run_file(path).await?;
    startup_timer.mark("runtime_run_file");
    emit_otter_stats_if_requested(&result);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "completion": result.completion_string(),
                "exitCode": result.exit_code()
            })
        );
    }
    let code = result.exit_code();
    // The process is exiting; freeing the multi-MB GC heap and unwinding every
    // interpreter Drop right before the kernel reclaims the address space is
    // pure latency. Hand the runtime to `forget` so a successful run skips that
    // teardown. Output is already flushed (console uses line-buffered
    // stdout/stderr) and the result has been consumed for the exit code.
    drop(result);
    std::mem::forget(otter);
    Ok(ExitCode::from(code))
}

#[allow(clippy::too_many_arguments)]
async fn run_file_with_cpu_profile(
    path: &std::path::Path,
    args: &[String],
    process_cwd: Option<&Path>,
    json: bool,
    caps: &CapabilitySet,
    startup_timer: &CliStartupTimer,
    profile_options: &CpuProfileOptions,
    max_heap_bytes: Option<u64>,
) -> Result<ExitCode, OtterError> {
    let mut builder = otter_runtime::Runtime::builder()
        .capabilities(caps.clone())
        .with_node_apis()
        .with_web_apis()
        .process_argv(process_argv_for_file(path, args))
        .module_loader(cli_loader_config_for_entry(path).await);
    if let Some(bytes) = max_heap_bytes {
        builder = builder.max_heap_bytes(bytes);
    }
    if let Some(cwd) = process_cwd {
        builder = builder.process_cwd(cwd.to_path_buf());
    }
    let mut runtime = builder.build()?;
    startup_timer.mark("runtime_build");
    runtime.enable_cpu_profiler(profile_options.interval);
    let result = runtime.run_file(path)?;
    startup_timer.mark("runtime_run_file");
    let profile = runtime
        .take_cpu_profile()
        .unwrap_or_else(|| otter_runtime::CpuProfile {
            interval: profile_options.interval.max(1),
            samples: Vec::new(),
            time_deltas_us: Vec::new(),
        });
    let artifacts = write_cpu_profile_artifacts(path, &profile, profile_options)?;
    eprintln!(
        "cpu profile written: {} ({} samples), {}",
        artifacts.cpuprofile.display(),
        profile.sample_count(),
        artifacts.folded.display()
    );
    emit_otter_stats_if_requested(&result);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "completion": result.completion_string(),
                "exitCode": result.exit_code(),
                "cpuProfile": {
                    "cpuprofile": artifacts.cpuprofile,
                    "folded": artifacts.folded,
                    "samples": profile.sample_count(),
                }
            })
        );
    }
    Ok(ExitCode::from(result.exit_code()))
}

#[derive(Debug, Clone)]
struct CpuProfileArtifacts {
    cpuprofile: PathBuf,
    folded: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CpuProfileFrameKey {
    function_name: String,
    module: String,
    line: u32,
    column: u32,
}

#[derive(Debug)]
struct CpuProfileNode {
    id: u32,
    key: CpuProfileFrameKey,
    hit_count: u32,
    children: BTreeMap<CpuProfileFrameKey, usize>,
}

fn write_cpu_profile_artifacts(
    entry_path: &Path,
    profile: &otter_runtime::CpuProfile,
    options: &CpuProfileOptions,
) -> Result<CpuProfileArtifacts, OtterError> {
    std::fs::create_dir_all(&options.dir).map_err(|err| pm_io_error(&options.dir, err))?;
    let base = cpu_profile_base_name(entry_path, options);
    let cpuprofile = options.dir.join(format!("{base}.cpuprofile"));
    let folded = options.dir.join(format!("{base}.folded"));
    write_chrome_cpu_profile(&cpuprofile, profile)?;
    write_folded_cpu_profile(&folded, profile)?;
    Ok(CpuProfileArtifacts { cpuprofile, folded })
}

fn cpu_profile_base_name(entry_path: &Path, options: &CpuProfileOptions) -> String {
    let raw = options.name.clone().unwrap_or_else(|| {
        entry_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("otter-profile")
            .to_string()
    });
    raw.strip_suffix(".cpuprofile")
        .or_else(|| raw.strip_suffix(".folded"))
        .unwrap_or(&raw)
        .to_string()
}

fn write_chrome_cpu_profile(
    path: &Path,
    profile: &otter_runtime::CpuProfile,
) -> Result<(), OtterError> {
    let mut nodes = vec![CpuProfileNode {
        id: 1,
        key: CpuProfileFrameKey {
            function_name: "(root)".to_string(),
            module: String::new(),
            line: 0,
            column: 0,
        },
        hit_count: 0,
        children: BTreeMap::new(),
    }];
    let mut sample_ids = Vec::with_capacity(profile.samples.len());
    for sample in &profile.samples {
        let mut current = 0usize;
        for frame in sample.iter().rev() {
            let key = CpuProfileFrameKey {
                function_name: frame.function_name.clone(),
                module: frame.module.clone(),
                line: 0,
                column: frame.span.0,
            };
            let child = if let Some(&idx) = nodes[current].children.get(&key) {
                idx
            } else {
                let idx = nodes.len();
                let id = u32::try_from(idx + 1).unwrap_or(u32::MAX);
                nodes.push(CpuProfileNode {
                    id,
                    key: key.clone(),
                    hit_count: 0,
                    children: BTreeMap::new(),
                });
                nodes[current].children.insert(key, idx);
                idx
            };
            current = child;
        }
        nodes[current].hit_count = nodes[current].hit_count.saturating_add(1);
        sample_ids.push(nodes[current].id);
    }
    let node_json = nodes
        .iter()
        .map(|node| {
            let children = node
                .children
                .values()
                .map(|&idx| nodes[idx].id)
                .collect::<Vec<_>>();
            serde_json::json!({
                "id": node.id,
                "callFrame": {
                    "functionName": node.key.function_name,
                    "scriptId": "0",
                    "url": node.key.module,
                    "lineNumber": node.key.line,
                    "columnNumber": node.key.column,
                },
                "hitCount": node.hit_count,
                "children": children,
            })
        })
        .collect::<Vec<_>>();
    let time_deltas = if profile.time_deltas_us.len() == sample_ids.len() {
        profile.time_deltas_us.clone()
    } else {
        vec![1; sample_ids.len()]
    };
    let end_time = time_deltas.iter().copied().sum::<u64>();
    let payload = serde_json::json!({
        "nodes": node_json,
        "startTime": 0,
        "endTime": end_time,
        "samples": sample_ids,
        "timeDeltas": time_deltas,
    });
    let file = std::fs::File::create(path).map_err(|err| pm_io_error(path, err))?;
    serde_json::to_writer_pretty(file, &payload).map_err(|err| OtterError::Internal {
        code: DiagnosticCode::DumpJson.as_str().to_string(),
        message: err.to_string(),
    })
}

fn write_folded_cpu_profile(
    path: &Path,
    profile: &otter_runtime::CpuProfile,
) -> Result<(), OtterError> {
    let mut folded: BTreeMap<String, u64> = BTreeMap::new();
    for sample in &profile.samples {
        let stack = if sample.is_empty() {
            "(idle)".to_string()
        } else {
            sample
                .iter()
                .rev()
                .map(|frame| folded_frame_name(&frame.function_name, &frame.module))
                .collect::<Vec<_>>()
                .join(";")
        };
        *folded.entry(stack).or_insert(0) += 1;
    }
    let mut out = std::fs::File::create(path).map_err(|err| pm_io_error(path, err))?;
    for (stack, count) in folded {
        writeln!(out, "{stack} {count}").map_err(|err| pm_io_error(path, err))?;
    }
    Ok(())
}

fn folded_frame_name(function_name: &str, module: &str) -> String {
    let mut name = function_name.replace(';', "\\;");
    if !module.is_empty() {
        name.push_str(" [");
        name.push_str(&module.replace(';', "\\;"));
        name.push(']');
    }
    name
}

fn emit_otter_stats_if_requested(result: &otter_runtime::ExecutionResult) {
    if std::env::var_os("OTTER_STATS").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let payload = serde_json::json!({
        "schema": "otter.stats.v1",
        "durationMs": result.duration.as_secs_f64() * 1000.0,
        "exitCode": result.exit_code(),
        "stats": result.stats(),
    });
    eprintln!("{payload}");
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunScriptInvocation {
    path: PathBuf,
    args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptCommandMode {
    Auto,
    Bin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScriptCommandTarget {
    mode: ScriptCommandMode,
    target: String,
    args: Vec<String>,
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
    let max_heap_bytes = args.max_heap_bytes;
    let cpu_profile = args.cpu_prof.then(|| CpuProfileOptions {
        dir: args.cpu_prof_dir.clone(),
        interval: args.cpu_prof_interval,
        name: args.cpu_prof_name.clone(),
    });
    match resolve_run_target(&project_root, &args).await? {
        RunTarget::File(path) => {
            run_file(
                &path,
                &target_args,
                json,
                dump_mode,
                caps,
                startup_timer,
                cpu_profile.as_ref(),
                max_heap_bytes,
            )
            .await
        }
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
            run_package_script(
                &project_root,
                &command,
                &target_args,
                json,
                caps,
                startup_timer,
                cpu_profile.as_ref(),
            )
            .await
        }
        RunTarget::Bin(bin) => {
            run_file(
                &bin.path,
                &target_args,
                json,
                dump_mode,
                caps,
                startup_timer,
                cpu_profile.as_ref(),
                max_heap_bytes,
            )
            .await
        }
    }
}

fn process_argv_for_file(path: &Path, args: &[String]) -> Vec<String> {
    let mut argv = Vec::with_capacity(args.len() + 2);
    argv.push(
        std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "otter".to_string()),
    );
    argv.push(path.to_string_lossy().to_string());
    argv.extend(args.iter().cloned());
    argv
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

    if let Some(path) = explicit_file_target(project_root, &args.target).await? {
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
        (None, None) => Ok(RunTarget::File(project_root.join(&args.target))),
    }
}

async fn explicit_file_target(
    base_dir: &Path,
    target: &str,
) -> Result<Option<PathBuf>, OtterError> {
    if let Some(path) = target.strip_prefix("file://") {
        return Ok(Some(PathBuf::from(path)));
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(None);
    }
    let path = PathBuf::from(target);
    let lookup_path = if path.is_absolute() {
        path.clone()
    } else {
        base_dir.join(&path)
    };
    let looks_like_path = path.is_absolute()
        || target.starts_with("./")
        || target.starts_with("../")
        || target.contains('/')
        || target.contains('\\');
    if looks_like_path
        || tokio::fs::try_exists(&lookup_path)
            .await
            .map_err(|err| pm_io_error(&lookup_path, err))?
    {
        Ok(Some(lookup_path))
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
    let graph = otter_pm::resolve_installed_project(project_root)
        .await
        .map_err(map_pm_error)?
        .graph;
    let bins = graph.resolve_bin(target);
    match bins {
        [] => Err(pm_config_error(format!(
            "unknown local package binary `{target}`"
        ))),
        [bin] => Ok(RunTarget::Bin(resolve_bin_source_path(&graph, bin))),
        many => Err(pm_config_error(format!(
            "ambiguous local package binary `{target}`\n  candidates:\n{}",
            many.iter()
                .map(|bin| format!("  - {} ({})", bin.path.display(), bin.package))
                .collect::<Vec<_>>()
                .join("\n")
        ))),
    }
}

fn resolve_bin_source_path(
    graph: &otter_pm::PackageGraph,
    bin: &otter_pm::PackageBin,
) -> otter_pm::PackageBin {
    let Some(package) = graph.package(&bin.package) else {
        return bin.clone();
    };
    let Some(bin_manifest) = &package.manifest.bin else {
        return bin.clone();
    };
    let source_path = match bin_manifest {
        PackageBinManifest::Path(path) => Some(package.root.join(path)),
        PackageBinManifest::Map(bins) => bins.get(&bin.name).map(|path| package.root.join(path)),
    }
    .filter(|path| path.exists());
    source_path.map_or_else(
        || bin.clone(),
        |path| otter_pm::PackageBin {
            package: bin.package.clone(),
            name: bin.name.clone(),
            path,
        },
    )
}

async fn run_package_script(
    project_root: &Path,
    command: &str,
    args: &[String],
    json: bool,
    caps: &CapabilitySet,
    startup_timer: &CliStartupTimer,
    cpu_profile: Option<&CpuProfileOptions>,
) -> Result<ExitCode, OtterError> {
    let invocation = resolve_package_script_invocation(project_root, command, args).await?;
    run_file_with_cwd(
        &invocation.path,
        &invocation.args,
        Some(project_root),
        json,
        None,
        caps,
        startup_timer,
        cpu_profile,
        None,
    )
    .await
}

async fn resolve_package_script_invocation(
    project_root: &Path,
    command: &str,
    forwarded_args: &[String],
) -> Result<RunScriptInvocation, OtterError> {
    let tokens = split_package_script_command(command)?;
    let mut target = package_script_command_target(command, &tokens)?;
    target.args.extend(forwarded_args.iter().cloned());

    if target.mode != ScriptCommandMode::Bin
        && let Some(path) = explicit_file_target(project_root, &target.target).await?
    {
        return Ok(RunScriptInvocation {
            path,
            args: target.args,
        });
    }

    let RunTarget::Bin(bin) = resolve_run_bin(project_root, &target.target).await? else {
        unreachable!("resolve_run_bin only returns RunTarget::Bin on success");
    };
    Ok(RunScriptInvocation {
        path: bin.path,
        args: target.args,
    })
}

fn package_script_command_target(
    command: &str,
    tokens: &[String],
) -> Result<ScriptCommandTarget, OtterError> {
    let Some(first) = tokens.first() else {
        return Err(pm_config_error("package script command is empty"));
    };
    let mut mode = ScriptCommandMode::Auto;
    let mut index = 0usize;
    if is_runtime_runner(first) {
        index += 1;
        if tokens.get(index).is_some_and(|token| token == "run") {
            index += 1;
        }
        match tokens.get(index).map(String::as_str) {
            Some("--bin") => {
                mode = ScriptCommandMode::Bin;
                index += 1;
            }
            Some("--script") => {
                return Err(pm_config_error(format!(
                    "package script `{command}` cannot dispatch another package script"
                )));
            }
            Some("--") => {
                index += 1;
            }
            Some(flag) if flag.starts_with('-') => {
                return Err(pm_config_error(format!(
                    "package script `{command}` uses unsupported runtime flag `{flag}`"
                )));
            }
            _ => {}
        }
    }

    let target = tokens.get(index).cloned().ok_or_else(|| {
        pm_config_error(format!(
            "package script `{command}` does not name a JS/TS file or local package bin"
        ))
    })?;
    let args = tokens.iter().skip(index + 1).cloned().collect();
    Ok(ScriptCommandTarget { mode, target, args })
}

fn is_runtime_runner(command: &str) -> bool {
    if matches!(command, "otter" | "otterjs" | "node") {
        return true;
    }
    Path::new(command)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| matches!(stem, "otter" | "otterjs" | "node"))
}

fn split_package_script_command(command: &str) -> Result<Vec<String>, OtterError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => current.push(c),
            (Some(_), '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                } else {
                    current.push('\\');
                }
            }
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => quote = Some(ch),
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                } else {
                    current.push('\\');
                }
            }
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }

    if let Some(q) = quote {
        return Err(pm_config_error(format!(
            "package script has unterminated {q} quote: `{command}`"
        )));
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
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
            "{}",
            serde_json::json!({
                "completion": result.completion_string(),
                "exitCode": result.exit_code()
            })
        );
    }
    Ok(ExitCode::from(result.exit_code()))
}

fn cli_otter_builder(caps: &CapabilitySet) -> otter_runtime::OtterBuilder {
    let mut builder = otter_runtime::Otter::builder()
        .capabilities(caps.clone())
        .with_node_apis()
        .with_web_apis();
    if let Some(target) = cli_trace_target() {
        builder = builder.tracer_factory(Some(trace_factory_for_target(&target)));
    }
    builder
}

/// Top-level `--trace[=<path>]` target installed by [`main`].
/// `None` disables tracing. `Some("-")` writes to stderr.
static CLI_TRACE_TARGET: std::sync::LazyLock<std::sync::Mutex<Option<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

fn install_cli_trace_target(target: Option<String>) {
    *CLI_TRACE_TARGET
        .lock()
        .expect("CLI trace target mutex poisoned") = target;
}

fn cli_trace_target() -> Option<String> {
    CLI_TRACE_TARGET
        .lock()
        .expect("CLI trace target mutex poisoned")
        .clone()
}

/// Build a [`otter_runtime::TracerFactory`] that emits the
/// VM-level step trace to the chosen sink. `-` writes to stderr;
/// any other string is treated as a file path that the factory
/// opens (truncating any existing contents) on the isolate thread
/// the first time the tracer is constructed.
fn trace_factory_for_target(target: &str) -> otter_runtime::TracerFactory {
    let target = target.to_string();
    otter_runtime::TracerFactory::new(move || -> Box<dyn otter_runtime::inspect::StepTracer> {
        let writer: Box<dyn io::Write> = if target == "-" {
            Box::new(io::BufWriter::new(io::stderr()))
        } else {
            match std::fs::File::create(&target) {
                Ok(file) => Box::new(io::BufWriter::new(file)),
                Err(err) => {
                    eprintln!(
                        "warning: --trace cannot open {target}: {err}; falling back to stderr"
                    );
                    Box::new(io::BufWriter::new(io::stderr()))
                }
            }
        };
        Box::new(otter_runtime::inspect::WriterTracer::new(writer))
    })
}

async fn cli_loader_config_for_entry(path: &Path) -> otter_runtime::module_loader::LoaderConfig {
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut config = otter_runtime::module_loader::LoaderConfig::new(base_dir);
    if let Some(project_root) = find_project_root_for_entry(path).await
        && let Ok(resolution) = otter_pm::resolve_installed_project(project_root).await
    {
        config.package_graph = Some(loader_graph_from_pm(&resolution.graph));
    }
    config
}

async fn find_project_root_for_entry(path: &Path) -> Option<PathBuf> {
    let mut cursor = match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_dir() => path.to_path_buf(),
        _ => path
            .parent()
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())?,
    };
    loop {
        let manifest = cursor.join(PACKAGE_JSON);
        match tokio::fs::try_exists(&manifest).await {
            Ok(true) => return Some(cursor),
            Ok(false) => {}
            Err(_) => return None,
        }
        if !cursor.pop() {
            return std::env::current_dir().ok();
        }
    }
}

fn loader_graph_from_pm(
    graph: &otter_pm::PackageGraph,
) -> otter_runtime::module_loader::LoaderPackageGraph {
    let mut loader_graph = otter_runtime::module_loader::LoaderPackageGraph::new();
    for package in graph.packages.values() {
        loader_graph.insert_package(otter_runtime::module_loader::LoaderPackageRoot {
            id: package.id.as_str().to_string(),
            name: package.name.clone(),
            version: package.version.clone(),
            root: package.root.clone(),
            main: package.manifest.main.clone(),
            module: package.manifest.module.clone(),
            exports: package.manifest.exports.clone(),
            imports: package.manifest.imports.clone(),
            package_type: package
                .manifest
                .package_type
                .map(|package_type| match package_type {
                    PackageType::Module => otter_runtime::module_loader::LoaderPackageType::Module,
                    PackageType::CommonJs => {
                        otter_runtime::module_loader::LoaderPackageType::CommonJs
                    }
                }),
        });
    }
    for (from, dependencies) in &graph.dependencies {
        for (name, target) in dependencies {
            let kind = graph
                .dependency_kind(from, name)
                .map(loader_dependency_kind_from_pm)
                .unwrap_or(otter_runtime::module_loader::LoaderPackageDependencyKind::Runtime);
            loader_graph.insert_dependency_with_kind(
                from.as_str().to_string(),
                name.clone(),
                target.as_str().to_string(),
                kind,
            );
        }
    }
    loader_graph
}

fn loader_dependency_kind_from_pm(
    kind: otter_pm::PackageDependencyKind,
) -> otter_runtime::module_loader::LoaderPackageDependencyKind {
    match kind {
        otter_pm::PackageDependencyKind::Runtime => {
            otter_runtime::module_loader::LoaderPackageDependencyKind::Runtime
        }
        otter_pm::PackageDependencyKind::Development => {
            otter_runtime::module_loader::LoaderPackageDependencyKind::Development
        }
        otter_pm::PackageDependencyKind::Peer => {
            otter_runtime::module_loader::LoaderPackageDependencyKind::Peer
        }
        otter_pm::PackageDependencyKind::Optional => {
            otter_runtime::module_loader::LoaderPackageDependencyKind::Optional
        }
    }
}

async fn run_check(
    path: &std::path::Path,
    json: bool,
    caps: &CapabilitySet,
) -> Result<ExitCode, OtterError> {
    let otter = cli_otter_builder(caps)
        .module_loader(cli_loader_config_for_entry(path).await)
        .build()?;
    otter.check_file(path).await?;
    if json {
        println!("{{\"ok\":true}}");
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_node_tests(
    args: TestArgs,
    json: bool,
    caps: &CapabilitySet,
    startup_timer: &CliStartupTimer,
) -> Result<ExitCode, OtterError> {
    let files = discover_node_test_files(&args.paths)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": "testPlan",
                "files": files,
            })
        );
    }

    let mut failed = false;
    for file in files {
        let otter = cli_otter_builder(caps)
            .process_argv(process_argv_for_file(&file, &[]))
            .module_loader(cli_loader_config_for_entry(&file).await)
            .build()?;
        startup_timer.mark("runtime_build");
        let result = otter.run_file(&file).await?;
        startup_timer.mark("runtime_run_file");
        let exit_code = result.exit_code();
        if exit_code != 0 {
            failed = true;
        }
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "type": "testFile",
                    "file": file,
                    "exitCode": exit_code,
                })
            );
        }
    }

    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn discover_node_test_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>, OtterError> {
    let roots = if paths.is_empty() {
        vec![PathBuf::from("test")]
    } else {
        paths.to_vec()
    };
    let mut files = Vec::new();
    for root in roots {
        let meta = std::fs::metadata(&root).map_err(|err| pm_io_error(&root, err))?;
        if meta.is_file() {
            files.push(root);
        } else if meta.is_dir() {
            collect_node_test_files(&root, &mut files)?;
        }
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        return Err(pm_config_error("no test files found"));
    }
    Ok(files)
}

fn collect_node_test_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), OtterError> {
    let mut entries = std::fs::read_dir(dir)
        .map_err(|err| pm_io_error(dir, err))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| pm_io_error(dir, err))?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let meta = entry.metadata().map_err(|err| pm_io_error(&path, err))?;
        if meta.is_dir() {
            collect_node_test_files(&path, out)?;
        } else if meta.is_file() && is_node_test_file(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_node_test_file(path: &Path) -> bool {
    if !has_node_test_extension(path) {
        return false;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with("test-")
        || name.starts_with("test_")
        || name.ends_with(".test.js")
        || name.ends_with(".test.cjs")
        || name.ends_with(".test.mjs")
        || name.ends_with(".test.ts")
        || name.ends_with(".test.cts")
        || name.ends_with(".test.mts")
        || path
            .components()
            .any(|part| part.as_os_str() == std::ffi::OsStr::new("test"))
}

fn has_node_test_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("js" | "cjs" | "mjs" | "ts" | "cts" | "mts")
    )
}

async fn run_dump(
    path: &std::path::Path,
    mode: &str,
    caps: &CapabilitySet,
    startup_timer: &CliStartupTimer,
) -> Result<ExitCode, OtterError> {
    let mut runtime = otter_runtime::Runtime::builder()
        .capabilities(caps.clone())
        .with_node_apis()
        .with_web_apis()
        .module_loader(cli_loader_config_for_entry(path).await)
        .build()?;
    startup_timer.mark("runtime_build");
    let compiled = runtime.dump_file(path)?;
    startup_timer.mark("runtime_dump_file");
    let text = match mode {
        "json" => compiled_dump_json(&compiled).map_err(|e| OtterError::Internal {
            code: DiagnosticCode::DumpJson.as_str().to_string(),
            message: e.to_string(),
        })?,
        _ => disassemble(&compiled.bytecode),
    };
    print!("{text}");
    Ok(ExitCode::SUCCESS)
}

fn compiled_dump_json(
    compiled: &otter_runtime::CompiledProgram,
) -> Result<String, serde_json::Error> {
    #[derive(serde::Serialize)]
    struct Dump<'a> {
        #[serde(rename = "otterBytecodeDumpVersion")]
        version: u32,
        #[serde(flatten)]
        bytecode: &'a otter_bytecode::BytecodeModule,
        metadata: &'a [otter_runtime::CompiledModuleMetadata],
        entry_url: Option<&'a str>,
    }

    let dump = Dump {
        version: otter_bytecode::dump::DUMP_SCHEMA_VERSION,
        bytecode: &compiled.bytecode,
        metadata: &compiled.metadata,
        entry_url: compiled.entry_url.as_deref(),
    };
    let mut text = serde_json::to_string_pretty(&dump)?;
    text.push('\n');
    Ok(text)
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
    let cache_root = root.join(".otter").join("cache");
    let report = otter_pm::install_local_project(
        root,
        &otter_pm::FsRegistryMetadataCache::new(cache_root.join("registry-metadata")),
        &otter_pm::HttpRegistryMetadataClient::new(),
        &otter_pm::FsPackageStore::new(cache_root),
        &otter_pm::HttpTarballClient::new(),
    )
    .await
    .map_err(map_pm_error)?;
    print_install_report(root, &report, json);
    Ok(ExitCode::SUCCESS)
}

fn print_install_report(root: &Path, report: &otter_pm::InstallReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "lockfile": root.join(otter_pm_lockfile::LOCKFILE_NAME),
                "lockfileChanged": report.lockfile_changed,
                "addedPackages": report.added_packages,
                "reusedPackages": report.reused_packages,
                "linkedBins": report.linked_bins,
                "lifecycleScripts": report.lifecycle_scripts,
                "importedLockfile": report.imported_lockfile.map(|format| format.filename())
            })
        );
    } else if report.lockfile_changed {
        if let Some(format) = report.imported_lockfile {
            println!("imported {}", root.join(format.filename()).display());
        }
        println!(
            "wrote {}",
            root.join(otter_pm_lockfile::LOCKFILE_NAME).display()
        );
        println!(
            "installed {} package{}, reused {}",
            report.added_packages,
            if report.added_packages == 1 { "" } else { "s" },
            report.reused_packages
        );
        println!(
            "linked {} bin{}",
            report.linked_bins,
            if report.linked_bins == 1 { "" } else { "s" }
        );
        println!(
            "ran {} lifecycle script{}",
            report.lifecycle_scripts,
            if report.lifecycle_scripts == 1 {
                ""
            } else {
                "s"
            }
        );
    } else {
        println!(
            "{} is up to date",
            root.join(otter_pm_lockfile::LOCKFILE_NAME).display()
        );
        println!(
            "installed {} package{}, reused {}",
            report.added_packages,
            if report.added_packages == 1 { "" } else { "s" },
            report.reused_packages
        );
        println!(
            "linked {} bin{}",
            report.linked_bins,
            if report.linked_bins == 1 { "" } else { "s" }
        );
        println!(
            "ran {} lifecycle script{}",
            report.lifecycle_scripts,
            if report.lifecycle_scripts == 1 {
                ""
            } else {
                "s"
            }
        );
    }
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
    let cache_root = args.root.join(".otter").join("cache");
    let report = otter_pm::install_local_project(
        &args.root,
        &otter_pm::FsRegistryMetadataCache::new(cache_root.join("registry-metadata")),
        &otter_pm::HttpRegistryMetadataClient::new(),
        &otter_pm::FsPackageStore::new(cache_root),
        &otter_pm::HttpTarballClient::new(),
    )
    .await
    .map_err(map_pm_error)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "added": added,
                "lockfileChanged": report.lockfile_changed,
                "addedPackages": report.added_packages,
                "reusedPackages": report.reused_packages,
                "linkedBins": report.linked_bins
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
        print_install_report(&args.root, &report, false);
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_pm_remove(args: RemoveArgs, json: bool) -> Result<ExitCode, OtterError> {
    if args.packages.is_empty() {
        return Err(pm_config_error(
            "otter remove requires at least one package",
        ));
    }
    let previous_lockfile = read_lockfile_if_present(&args.root).await?;
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
    let current_lockfile = read_lockfile_if_present(&args.root)
        .await?
        .unwrap_or_else(otter_pm_lockfile::Lockfile::new);
    let prune = if let Some(previous_lockfile) = previous_lockfile {
        otter_pm::prune_removed_registry_packages(&args.root, &previous_lockfile, &current_lockfile)
            .await
            .map_err(map_pm_error)?
    } else {
        otter_pm::PruneReport {
            removed_packages: 0,
            removed_bins: 0,
        }
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "removed": removed,
                "lockfileChanged": lockfile_changed,
                "removedPackages": prune.removed_packages,
                "removedBins": prune.removed_bins
            })
        );
    } else {
        println!(
            "removed {removed} dependency entr{}",
            if removed == 1 { "y" } else { "ies" }
        );
        println!(
            "pruned {} package{}, {} bin{}",
            prune.removed_packages,
            if prune.removed_packages == 1 { "" } else { "s" },
            prune.removed_bins,
            if prune.removed_bins == 1 { "" } else { "s" }
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OutdatedPackage {
    name: String,
    bucket: &'static str,
    current: String,
    wanted: String,
    latest: String,
    range: String,
    bump: VersionBump,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum VersionBump {
    Patch,
    Minor,
    Major,
    Unknown,
}

impl VersionBump {
    fn label(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::Major => "major",
            Self::Unknown => "unknown",
        }
    }
}

async fn run_pm_outdated(args: OutdatedArgs, json: bool) -> Result<ExitCode, OtterError> {
    let manifest = PackageManifest::read_from_dir(&args.root)
        .await
        .map_err(map_manifest_error)?;
    let lockfile = read_lockfile_if_present(&args.root)
        .await?
        .unwrap_or_else(otter_pm_lockfile::Lockfile::new);
    let cache_root = args.root.join(".otter").join("cache");
    let cache = otter_pm::FsRegistryMetadataCache::new(cache_root.join("registry-metadata"));
    let client = otter_pm::HttpRegistryMetadataClient::new();
    let rows = collect_outdated_packages(&manifest, &lockfile, &cache, &client, &args).await?;
    print_outdated_report(&rows, json);
    Ok(if rows.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

async fn collect_outdated_packages(
    manifest: &PackageManifest,
    lockfile: &Lockfile,
    cache: &otter_pm::FsRegistryMetadataCache,
    client: &impl otter_pm::RegistryMetadataClient,
    args: &OutdatedArgs,
) -> Result<Vec<OutdatedPackage>, OtterError> {
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for (bucket, dependencies) in manifest.dependency_buckets() {
        if !outdated_includes_bucket(args, bucket) {
            continue;
        }
        for (name, range) in dependencies {
            if !seen.insert(name.clone()) || !is_registry_range(range) {
                continue;
            }
            let metadata = cache
                .get_or_fetch(name, client)
                .await
                .map_err(map_pm_error)?;
            let current =
                current_locked_version(lockfile, name).unwrap_or_else(|| "<missing>".to_string());
            let wanted =
                wanted_registry_version(&metadata, range).unwrap_or_else(|| current.clone());
            let latest = latest_registry_version(&metadata).unwrap_or_else(|| wanted.clone());
            if current == wanted && current == latest {
                continue;
            }
            rows.push(OutdatedPackage {
                name: name.clone(),
                bucket,
                current: current.clone(),
                wanted: wanted.clone(),
                latest: latest.clone(),
                range: range.clone(),
                bump: classify_bump(&current, &latest),
            });
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.bucket.cmp(b.bucket)));
    Ok(rows)
}

fn outdated_includes_bucket(args: &OutdatedArgs, bucket: &str) -> bool {
    match bucket {
        "dependencies" => true,
        "devDependencies" => args.dev,
        "peerDependencies" => args.peer,
        "optionalDependencies" => args.optional,
        _ => false,
    }
}

fn is_registry_range(range: &str) -> bool {
    !(range.starts_with("workspace:")
        || range.starts_with("file:")
        || range.starts_with("http://")
        || range.starts_with("https://")
        || range.ends_with(".tgz")
        || range.ends_with(".tar.gz"))
}

fn current_locked_version(lockfile: &Lockfile, name: &str) -> Option<String> {
    lockfile
        .packages
        .values()
        .find(|package| package.name == name)
        .map(|package| package.version.clone())
}

fn wanted_registry_version(
    metadata: &otter_pm::NpmRegistryMetadata,
    range: &str,
) -> Option<String> {
    if let Some(version) = metadata.versions.get(range) {
        return Some(version.version.clone());
    }
    let req = normalize_npm_range_for_cli(range)
        .and_then(|normalized| VersionReq::parse(&normalized).ok())?;
    let mut versions = semver_versions(metadata);
    versions.retain(|version| req.matches(version));
    versions.pop().map(|version| version.to_string())
}

fn latest_registry_version(metadata: &otter_pm::NpmRegistryMetadata) -> Option<String> {
    metadata
        .dist_tags
        .get("latest")
        .filter(|version| metadata.versions.contains_key(*version))
        .cloned()
        .or_else(|| {
            semver_versions(metadata)
                .pop()
                .map(|version| version.to_string())
        })
}

fn semver_versions(metadata: &otter_pm::NpmRegistryMetadata) -> Vec<Version> {
    let mut versions = metadata
        .versions
        .keys()
        .filter_map(|version| Version::parse(version).ok())
        .collect::<Vec<_>>();
    versions.sort();
    versions
}

fn normalize_npm_range_for_cli(range: &str) -> Option<String> {
    let trimmed = range.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed {
        "*" | "latest" => Some("*".to_string()),
        value if value.starts_with('^') || value.starts_with('~') => Some(value.to_string()),
        value if value.chars().next().is_some_and(|c| c.is_ascii_digit()) => {
            Some(format!("={value}"))
        }
        value => Some(value.to_string()),
    }
}

fn classify_bump(current: &str, latest: &str) -> VersionBump {
    let (Ok(current), Ok(latest)) = (Version::parse(current), Version::parse(latest)) else {
        return VersionBump::Unknown;
    };
    if latest.major != current.major {
        VersionBump::Major
    } else if latest.minor != current.minor {
        VersionBump::Minor
    } else if latest.patch != current.patch {
        VersionBump::Patch
    } else {
        VersionBump::Unknown
    }
}

fn print_outdated_report(rows: &[OutdatedPackage], json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": rows.is_empty(),
                "outdated": rows.iter().map(|row| serde_json::json!({
                    "name": row.name,
                    "bucket": row.bucket,
                    "current": row.current,
                    "wanted": row.wanted,
                    "latest": row.latest,
                    "range": row.range,
                    "bump": row.bump.label()
                })).collect::<Vec<_>>()
            })
        );
        return;
    }
    if rows.is_empty() {
        println!("all dependencies are up to date");
        return;
    }
    print!("{}", render_outdated_table(rows));
}

fn render_outdated_table(rows: &[OutdatedPackage]) -> String {
    let headers = [
        "Package",
        "Current",
        "Wanted",
        "Latest",
        "Bump",
        "Dependency",
    ];
    let mut widths = headers.map(str::len);
    for row in rows {
        let values = [
            row.name.as_str(),
            row.current.as_str(),
            row.wanted.as_str(),
            row.latest.as_str(),
            row.bump.label(),
            row.bucket,
        ];
        for (idx, value) in values.iter().enumerate() {
            widths[idx] = widths[idx].max(value.len());
        }
    }
    let mut table = String::new();
    push_table_border(&mut table, "┌", "┬", "┐", &widths);
    push_table_row(&mut table, &headers, &widths);
    push_table_border(&mut table, "├", "┼", "┤", &widths);
    for row in rows {
        let values = [
            row.name.as_str(),
            row.current.as_str(),
            row.wanted.as_str(),
            row.latest.as_str(),
            row.bump.label(),
            row.bucket,
        ];
        push_table_row(&mut table, &values, &widths);
    }
    push_table_border(&mut table, "└", "┴", "┘", &widths);
    table
}

fn push_table_row(table: &mut String, values: &[&str; 6], widths: &[usize; 6]) {
    table.push('│');
    for (idx, value) in values.iter().enumerate() {
        write!(table, " {:<width$} │", value, width = widths[idx]).expect("write table row");
    }
    table.push('\n');
}

fn push_table_border(
    table: &mut String,
    left: &str,
    junction: &str,
    right: &str,
    widths: &[usize; 6],
) {
    table.push_str(left);
    for (idx, width) in widths.iter().enumerate() {
        table.push_str(&"─".repeat(width + 2));
        table.push_str(if idx + 1 == widths.len() {
            right
        } else {
            junction
        });
    }
    table.push('\n');
}

async fn read_lockfile_if_present(root: &Path) -> Result<Option<Lockfile>, OtterError> {
    for (path, format) in otter_pm_lockfile::project_lockfile_candidates(root) {
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(pm_io_error(&path, err)),
        };
        return Lockfile::parse_format(format, &text)
            .map(Some)
            .map_err(map_lockfile_error);
    }
    Ok(None)
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
    let split = if let Some(rest) = spec.strip_prefix('@') {
        rest.rfind('@').map(|index| index + 1)
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

fn map_lockfile_error(err: otter_pm_lockfile::LockfileError) -> OtterError {
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

    #[test]
    fn pm_graph_adapter_preserves_dependency_edge_kinds() {
        let mut graph = otter_pm::PackageGraph::new();
        let app_id = otter_pm::PackageId::root_workspace("app");
        let dev_id = otter_pm::PackageId::registry("dev-tool", "^1.0.0");
        graph.insert_package(otter_pm::PackageRoot {
            id: app_id.clone(),
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            root: PathBuf::from("/app"),
            manifest: PackageManifest::default(),
        });
        graph.insert_package(otter_pm::PackageRoot {
            id: dev_id.clone(),
            name: "dev-tool".to_string(),
            version: "1.0.0".to_string(),
            root: PathBuf::from("/app/node_modules/dev-tool"),
            manifest: PackageManifest::default(),
        });
        graph.insert_dependency_with_kind(
            app_id.clone(),
            "dev-tool",
            dev_id,
            otter_pm::PackageDependencyKind::Development,
        );

        let loader_graph = loader_graph_from_pm(&graph);

        assert_eq!(
            loader_graph.dependency_kind(app_id.as_str(), "dev-tool"),
            Some(otter_runtime::module_loader::LoaderPackageDependencyKind::Development)
        );
    }

    #[test]
    fn outdated_semver_report_distinguishes_wanted_latest_and_bump() {
        let manifest =
            PackageManifest::parse_json(r#"{"dependencies":{"alpha":"^1.0.0","beta":"~1.2.0"}}"#)
                .unwrap();
        let lockfile = Lockfile::parse_toml(
            r#"lockfile_version = 1

[packages."alpha@npm:^1.0.0"]
name = "alpha"
version = "1.0.0"
integrity = "sha512-test"

[packages."beta@npm:~1.2.0"]
name = "beta"
version = "1.2.0"
integrity = "sha512-test"
"#,
        )
        .unwrap();
        let args = OutdatedArgs {
            root: PathBuf::from("."),
            dev: false,
            peer: false,
            optional: false,
        };
        let tmp = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cache = otter_pm::FsRegistryMetadataCache::new(tmp.path());
        rt.block_on(async {
            cache
                .write(&registry_metadata(
                    "alpha",
                    "2.0.0",
                    ["1.0.0", "1.1.0", "2.0.0"],
                ))
                .await
                .unwrap();
            cache
                .write(&registry_metadata(
                    "beta",
                    "1.2.3",
                    ["1.2.0", "1.2.3", "1.3.0"],
                ))
                .await
                .unwrap();
        });
        let client = otter_pm::FileRegistryMetadataClient::new(tmp.path());

        let rows = rt
            .block_on(collect_outdated_packages(
                &manifest, &lockfile, &cache, &client, &args,
            ))
            .unwrap();

        assert_eq!(rows.len(), 2);
        let alpha = rows.iter().find(|row| row.name == "alpha").unwrap();
        assert_eq!(alpha.current, "1.0.0");
        assert_eq!(alpha.wanted, "1.1.0");
        assert_eq!(alpha.latest, "2.0.0");
        assert_eq!(alpha.bump, VersionBump::Major);
        let beta = rows.iter().find(|row| row.name == "beta").unwrap();
        assert_eq!(beta.wanted, "1.2.3");
        assert_eq!(beta.latest, "1.2.3");
        assert_eq!(beta.bump, VersionBump::Patch);

        let table = render_outdated_table(&rows);
        assert!(table.contains("┌"));
        assert!(table.contains("│ Package"));
        assert!(table.contains("│ alpha"));
        assert!(table.contains("major"));
        assert!(table.contains("└"));
    }

    #[tokio::test]
    async fn cli_lockfile_reader_accepts_package_lock_for_migration() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join("package-lock.json"),
            r#"{
  "name": "app",
  "version": "0.1.0",
  "lockfileVersion": 3,
  "packages": {
    "": {
      "name": "app",
      "version": "0.1.0",
      "dependencies": {
        "tool": "^1.0.0"
      }
    },
    "node_modules/tool": {
      "version": "1.2.0",
      "resolved": "https://registry.npmjs.org/tool/-/tool-1.2.0.tgz",
      "integrity": "sha512-tool"
    }
  }
}"#,
        )
        .await
        .unwrap();

        let lockfile = read_lockfile_if_present(tmp.path()).await.unwrap().unwrap();
        assert_eq!(
            lockfile
                .packages
                .get("tool@npm:^1.0.0")
                .map(|package| package.version.as_str()),
            Some("1.2.0")
        );
    }

    fn registry_metadata<const N: usize>(
        name: &str,
        latest: &str,
        versions: [&str; N],
    ) -> otter_pm::NpmRegistryMetadata {
        let mut dist_tags = std::collections::BTreeMap::new();
        dist_tags.insert("latest".to_string(), latest.to_string());
        let versions = versions
            .into_iter()
            .map(|version| {
                (
                    version.to_string(),
                    otter_pm::NpmPackageVersion {
                        name: name.to_string(),
                        version: version.to_string(),
                        dependencies: Default::default(),
                        peer_dependencies: Default::default(),
                        optional_dependencies: Default::default(),
                        bin: None,
                        scripts: Default::default(),
                        dist: Default::default(),
                    },
                )
            })
            .collect();
        otter_pm::NpmRegistryMetadata {
            name: name.to_string(),
            dist_tags,
            versions,
        }
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
        tokio::fs::create_dir_all(tmp.path().join("tools/file-tool"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("tools/file-tool/package.json"),
            r#"{"name":"file-tool","version":"1.0.0"}"#,
        )
        .await
        .unwrap();
        run_pm_add(
            AddArgs {
                root: tmp.path().to_path_buf(),
                dev: false,
                peer: false,
                optional: false,
                packages: vec!["file-tool@file:tools/file-tool".to_string()],
            },
            true,
        )
        .await
        .unwrap();
        let manifest = tokio::fs::read_to_string(tmp.path().join(PACKAGE_JSON))
            .await
            .unwrap();
        assert!(manifest.contains("\"file-tool\": \"file:tools/file-tool\""));
        let lockfile = tokio::fs::read_to_string(tmp.path().join(otter_pm_lockfile::LOCKFILE_NAME))
            .await
            .unwrap();
        assert!(lockfile.contains("file-tool@file:tools/file-tool"));
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
            cpu_prof: false,
            cpu_prof_dir: PathBuf::from("/tmp/otter-prof"),
            cpu_prof_interval: 1000,
            cpu_prof_name: None,
            max_heap_bytes: None,
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
            cpu_prof: false,
            cpu_prof_dir: PathBuf::from("/tmp/otter-prof"),
            cpu_prof_interval: 1000,
            cpu_prof_name: None,
            max_heap_bytes: None,
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
            cpu_prof: false,
            cpu_prof_dir: PathBuf::from("/tmp/otter-prof"),
            cpu_prof_interval: 1000,
            cpu_prof_name: None,
            max_heap_bytes: None,
            args: Vec::new(),
        };
        let resolved = resolve_run_target(tmp.path(), &args).await.unwrap();
        match resolved {
            RunTarget::Bin(bin) => assert!(bin.path.ends_with("packages/tool/tool.ts")),
            other => panic!("expected bin, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_target_force_bin_resolves_installed_registry_bin() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"tool":"^1.0.0"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join(otter_pm_lockfile::LOCKFILE_NAME),
            r#"lockfile_version = 1

[packages."tool@npm:^1.0.0"]
name = "tool"
version = "1.0.0"
integrity = "sha512-test"

[packages."tool@npm:^1.0.0".resolved]
kind = "registry"
reference = "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz"

[packages."tool@npm:^1.0.0".lifecycle]
trust = "untrusted"
"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules/tool"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/package.json"),
            r#"{"name":"tool","version":"1.0.0","bin":{"tool":"./tool.ts"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules/.bin"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("node_modules/.bin/tool"), "undefined;")
            .await
            .unwrap();
        let args = RunArgs {
            target: "tool".to_string(),
            script: false,
            bin: true,
            cpu_prof: false,
            cpu_prof_dir: PathBuf::from("/tmp/otter-prof"),
            cpu_prof_interval: 1000,
            cpu_prof_name: None,
            max_heap_bytes: None,
            args: Vec::new(),
        };
        let resolved = resolve_run_target(tmp.path(), &args).await.unwrap();
        match resolved {
            RunTarget::Bin(bin) => assert!(bin.path.ends_with("node_modules/.bin/tool")),
            other => panic!("expected bin, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_installed_registry_bin_resolves_relative_imports_from_package_root() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"tool":"^1.0.0"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join(otter_pm_lockfile::LOCKFILE_NAME),
            r#"lockfile_version = 1

[packages."tool@npm:^1.0.0"]
name = "tool"
version = "1.0.0"
integrity = "sha512-test"

[packages."tool@npm:^1.0.0".resolved]
kind = "registry"
reference = "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz"

[packages."tool@npm:^1.0.0".lifecycle]
trust = "untrusted"
"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules/tool"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/package.json"),
            r#"{"name":"tool","version":"1.0.0","type":"module","bin":{"tool":"./tool.ts"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/tool.ts"),
            r#"import { value } from "./helper.ts";
function fail() { return undefined.x; }
if (value !== 53) fail();
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/helper.ts"),
            "export let value = 53;\n",
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules/.bin"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("node_modules/.bin/tool"), "undefined;")
            .await
            .unwrap();

        let resolved = resolve_run_bin(tmp.path(), "tool").await.unwrap();
        let bin = match resolved {
            RunTarget::Bin(bin) => bin,
            other => panic!("expected bin, got {other:?}"),
        };
        let startup_timer = CliStartupTimer::from_env();
        let code = run_file(
            &bin.path,
            &[],
            false,
            None,
            &CapabilitySet::default(),
            &startup_timer,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[tokio::test]
    async fn run_file_forwards_process_argv() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = tmp.path().join("argv.ts");
        tokio::fs::write(
            &entry,
            r#"
if (process.argv[1].indexOf("argv.ts") === -1) throw new Error("missing entry");
if (process.argv[2] !== "alpha") throw new Error("missing first arg");
if (process.argv[3] !== "two words") throw new Error("missing second arg");
"#,
        )
        .await
        .unwrap();
        let startup_timer = CliStartupTimer::from_env();
        let code = run_file(
            &entry,
            &["alpha".to_string(), "two words".to_string()],
            false,
            None,
            &CapabilitySet::default(),
            &startup_timer,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[tokio::test]
    async fn run_file_returns_process_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let entry = tmp.path().join("exit.ts");
        tokio::fs::write(&entry, "process.exit(12); throw new Error('unreachable');")
            .await
            .unwrap();
        let startup_timer = CliStartupTimer::from_env();
        let code = run_file(
            &entry,
            &[],
            false,
            None,
            &CapabilitySet::default(),
            &startup_timer,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(code, ExitCode::from(12));
    }

    #[test]
    fn package_script_command_split_preserves_quoted_args() {
        assert_eq!(
            split_package_script_command("otter run ./task.ts 'two words' \"and three\"").unwrap(),
            ["otter", "run", "./task.ts", "two words", "and three"]
        );
    }

    #[tokio::test]
    async fn package_script_runs_file_through_runtime_with_args_and_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let scripts_dir = tmp.path().join("scripts");
        tokio::fs::create_dir_all(&scripts_dir).await.unwrap();
        let entry = scripts_dir.join("build.ts");
        let cwd_literal = serde_json::to_string(&tmp.path().to_string_lossy()).unwrap();
        tokio::fs::write(
            &entry,
            format!(
                r#"
function fail() {{ process.exit(31); }}
if (process.cwd() !== {cwd_literal}) fail();
if (process.argv[1].indexOf("build.ts") === -1) fail();
if (process.argv[2] !== "from-script") fail();
if (process.argv[3] !== "from-cli") fail();
"#
            ),
        )
        .await
        .unwrap();
        let startup_timer = CliStartupTimer::from_env();
        let code = run_package_script(
            tmp.path(),
            "scripts/build.ts from-script",
            &["from-cli".to_string()],
            false,
            &CapabilitySet::default(),
            &startup_timer,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[tokio::test]
    async fn package_script_runs_local_bin_through_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","workspaces":["packages/*"],"scripts":{"tool":"otter run --bin tool from-script"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("packages/tool"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("packages/tool/package.json"),
            r#"{"name":"tool","version":"1.0.0","type":"module","bin":"./tool.ts"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("packages/tool/tool.ts"),
            r#"
function fail() { process.exit(32); }
if (process.argv[1].indexOf("tool.ts") === -1) fail();
if (process.argv[2] !== "from-script") fail();
if (process.argv[3] !== "from-cli") fail();
"#,
        )
        .await
        .unwrap();
        let manifest = PackageManifest::read_from_dir(tmp.path()).await.unwrap();
        let command = manifest.scripts.get("tool").unwrap().clone();
        let startup_timer = CliStartupTimer::from_env();
        let code = run_package_script(
            tmp.path(),
            &command,
            &["from-cli".to_string()],
            false,
            &CapabilitySet::default(),
            &startup_timer,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[tokio::test]
    async fn package_script_rejects_unknown_shell_command() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join(PACKAGE_JSON), r#"{"name":"app"}"#)
            .await
            .unwrap();
        let err = run_package_script(
            tmp.path(),
            "echo hello",
            &[],
            false,
            &CapabilitySet::default(),
            &CliStartupTimer::from_env(),
            None,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("unknown local package binary `echo`")
        );
    }

    #[tokio::test]
    async fn check_uses_installed_package_graph_for_bare_imports() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"tool":"^1.0.0"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("entry.ts"),
            r#"import { value } from "tool";
function fail() { return undefined.x; }
fail();
value;
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join(otter_pm_lockfile::LOCKFILE_NAME),
            r#"lockfile_version = 1

[packages."tool@npm:^1.0.0"]
name = "tool"
version = "1.0.0"
integrity = "sha512-test"

[packages."tool@npm:^1.0.0".resolved]
kind = "registry"
reference = "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz"

[packages."tool@npm:^1.0.0".lifecycle]
trust = "untrusted"
"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules/tool"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/package.json"),
            r#"{"name":"tool","version":"1.0.0","main":"index.js"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/index.js"),
            "export let value = 17;\n",
        )
        .await
        .unwrap();

        run_check(
            &tmp.path().join("entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn check_uses_package_json_imports_from_pm_graph() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r##"{
              "name":"app",
              "type":"module",
              "imports":{
                "#alias":{
                  "otter":"./src/otter.ts",
                  "default":"./src/default.ts"
                }
              }
            }"##,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("entry.ts"),
            r##"import { value } from "#alias";
function fail() { return undefined.x; }
if (value !== 23) fail();
"##,
        )
        .await
        .unwrap();
        tokio::fs::write(tmp.path().join("src/otter.ts"), "export let value = 23;\n")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("src/default.ts"), "export let value = 0;\n")
            .await
            .unwrap();

        run_check(
            &tmp.path().join("entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn installed_package_uses_own_imports_from_pm_graph() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"tool":"^1.0.0"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("entry.ts"),
            r#"import { value } from "tool";
function fail() { return undefined.x; }
if (value !== 31) fail();
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join(otter_pm_lockfile::LOCKFILE_NAME),
            r#"lockfile_version = 1

[packages."tool@npm:^1.0.0"]
name = "tool"
version = "1.0.0"
integrity = "sha512-test"

[packages."tool@npm:^1.0.0".resolved]
kind = "registry"
reference = "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz"

[packages."tool@npm:^1.0.0".lifecycle]
trust = "untrusted"
"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules/tool"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/package.json"),
            r##"{
              "name":"tool",
              "version":"1.0.0",
              "main":"index.js",
              "imports":{
                "#internal":{
                  "otter":"./otter.js",
                  "default":"./default.js"
                }
              }
            }"##,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/index.js"),
            r##"import { internal } from "#internal";
export let value = internal;
"##,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/otter.js"),
            "export let internal = 31;\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/default.js"),
            "export let internal = 0;\n",
        )
        .await
        .unwrap();

        run_check(
            &tmp.path().join("entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pm_graph_blocks_undeclared_disk_package_imports() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join(PACKAGE_JSON), r#"{"name":"app"}"#)
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("entry.ts"), r#"import "hidden";"#)
            .await
            .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules/hidden"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/hidden/package.json"),
            r#"{"name":"hidden","version":"1.0.0","main":"index.js"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/hidden/index.js"),
            "undefined;\n",
        )
        .await
        .unwrap();

        let err = run_check(
            &tmp.path().join("entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .expect_err("undeclared disk package must be blocked by PM graph");

        match err {
            OtterError::Compile { diagnostics } => {
                assert!(diagnostics.iter().any(|diagnostic| {
                    diagnostic
                        .message
                        .contains("does not declare dependency `hidden`")
                }));
            }
            other => panic!("expected compile diagnostic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn optional_dependency_missing_reports_pm_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","optionalDependencies":{"maybe-native":"^1.0.0"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(tmp.path().join("entry.ts"), r#"import "maybe-native";"#)
            .await
            .unwrap();

        let err = run_check(
            &tmp.path().join("entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .expect_err("missing optional dependency should report PM diagnostic");

        match err {
            OtterError::Compile { diagnostics } => {
                assert!(
                    diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.message.contains(
                            "optional dependency `maybe-native` for package `app` is not installed"
                        ))
                );
            }
            other => panic!("expected compile diagnostic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn peer_dependency_missing_reports_pm_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","peerDependencies":{"react":"^19.0.0"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(tmp.path().join("entry.ts"), r#"import "react";"#)
            .await
            .unwrap();

        let err = run_check(
            &tmp.path().join("entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .expect_err("missing peer dependency should report PM diagnostic");

        match err {
            OtterError::Compile { diagnostics } => {
                assert!(diagnostics.iter().any(|diagnostic| {
                    diagnostic
                        .message
                        .contains("peer dependency `react` for package `app` is not installed")
                }));
            }
            other => panic!("expected compile diagnostic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn installed_package_peer_dependency_uses_available_project_package() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"plugin":"^1.0.0","react":"^19.0.0"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("entry.ts"),
            r#"import { value } from "plugin";
function fail() { return undefined.x; }
if (value !== 61) fail();
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join(otter_pm_lockfile::LOCKFILE_NAME),
            r#"lockfile_version = 1

[packages."plugin@npm:^1.0.0"]
name = "plugin"
version = "1.0.0"
integrity = "sha512-test"

[packages."plugin@npm:^1.0.0".dependencies]
react = "react@npm:^18.0.0"

[packages."plugin@npm:^1.0.0".resolved]
kind = "registry"
reference = "https://registry.npmjs.org/plugin/-/plugin-1.0.0.tgz"

[packages."plugin@npm:^1.0.0".lifecycle]
trust = "untrusted"

[packages."react@npm:^19.0.0"]
name = "react"
version = "19.0.0"
integrity = "sha512-test"

[packages."react@npm:^19.0.0".resolved]
kind = "registry"
reference = "https://registry.npmjs.org/react/-/react-19.0.0.tgz"

[packages."react@npm:^19.0.0".lifecycle]
trust = "untrusted"
"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules/plugin"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/plugin/package.json"),
            r#"{"name":"plugin","version":"1.0.0","main":"index.js","peerDependencies":{"react":"^18.0.0"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/plugin/index.js"),
            r#"import { answer } from "react";
export let value = answer;
"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules/react"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/react/package.json"),
            r#"{"name":"react","version":"19.0.0","main":"index.js"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/react/index.js"),
            "export let answer = 61;\n",
        )
        .await
        .unwrap();

        run_check(
            &tmp.path().join("entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn installed_package_can_self_reference_without_dependency_edge() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join(PACKAGE_JSON),
            r#"{"name":"app","dependencies":{"tool":"^1.0.0"}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("entry.ts"),
            r#"import { value } from "tool";
function fail() { return undefined.x; }
if (value !== 41) fail();
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join(otter_pm_lockfile::LOCKFILE_NAME),
            r#"lockfile_version = 1

[packages."tool@npm:^1.0.0"]
name = "tool"
version = "1.0.0"
integrity = "sha512-test"

[packages."tool@npm:^1.0.0".resolved]
kind = "registry"
reference = "https://registry.npmjs.org/tool/-/tool-1.0.0.tgz"

[packages."tool@npm:^1.0.0".lifecycle]
trust = "untrusted"
"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(tmp.path().join("node_modules/tool"))
            .await
            .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/package.json"),
            r#"{"name":"tool","version":"1.0.0","main":"index.js"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/index.js"),
            r#"import { inner } from "tool/self.js";
export let value = inner;
"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("node_modules/tool/self.js"),
            "export let inner = 41;\n",
        )
        .await
        .unwrap();

        run_check(
            &tmp.path().join("entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .unwrap();
    }

    #[test]
    fn direct_file_shorthand_parses_as_positional_without_run_subcommand() {
        let cli = Cli::try_parse_from(["otter", "app.ts"]).expect("parse shorthand");
        assert!(cli.command.is_none());
        assert_eq!(cli.args, vec!["app.ts".to_string()]);
    }

    #[test]
    fn test_command_parses_node_test_paths() {
        let cli =
            Cli::try_parse_from(["otter", "test", "test/app.test.ts"]).expect("parse test command");
        match cli.command {
            Some(Command::Test(args)) => {
                assert_eq!(args.paths, vec![PathBuf::from("test/app.test.ts")]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn node_test_discovery_uses_default_test_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        std::fs::create_dir_all("test/nested").unwrap();
        std::fs::write("test/app.test.js", "test('a', () => {});\n").unwrap();
        std::fs::write("test/nested/helper.txt", "nope\n").unwrap();

        let files = discover_node_test_files(&[]).unwrap();
        std::env::set_current_dir(previous).unwrap();

        assert_eq!(files, vec![PathBuf::from("test/app.test.js")]);
    }

    #[tokio::test]
    async fn fixture_project_covers_development_loop_resolution() {
        let fixture = workspace_root()
            .join("tests")
            .join("fixtures")
            .join("pkg")
            .join("development-loop");
        // `node_modules` is gitignored, so copy the committed fixture
        // into a scratch root and materialize the installed registry
        // package there — the test stays hermetic and runs identically
        // on a fresh checkout.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("development-loop");
        copy_fixture_tree(&fixture, &root);
        let tool = root.join("node_modules/fixture-tool");
        write_fixture(
            &tool.join("package.json"),
            r#"{
  "name": "fixture-tool",
  "version": "1.0.0",
  "type": "module",
  "main": "index.js",
  "exports": {
    ".": {
      "otter": "./otter.js",
      "default": "./index.js"
    }
  },
  "bin": {
    "fixture-tool": "./bin.js"
  }
}
"#,
        );
        write_fixture(&tool.join("bin.js"), "undefined;\n");
        write_fixture(
            &tool.join("index.js"),
            "export const installedValue = 20;\n",
        );
        write_fixture(&tool.join("otter.js"), "export const installedValue = 2;\n");
        write_fixture(&root.join("node_modules/.bin/fixture-tool"), "undefined;\n");
        let entry = root.join("entry.ts");

        run_check(&entry, false, &CapabilitySet::default())
            .await
            .unwrap();

        let args = RunArgs {
            target: "fixture-tool".to_string(),
            script: false,
            bin: true,
            cpu_prof: false,
            cpu_prof_dir: PathBuf::from("/tmp/otter-prof"),
            cpu_prof_interval: 1000,
            cpu_prof_name: None,
            max_heap_bytes: None,
            args: Vec::new(),
        };
        let resolved = resolve_run_target(&root, &args).await.unwrap();
        match resolved {
            RunTarget::Bin(bin) => assert!(bin.path.ends_with("node_modules/fixture-tool/bin.js")),
            other => panic!("expected fixture-tool bin, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn diagnostics_snapshots_cover_development_loop_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let startup_timer = CliStartupTimer::from_env();

        write_fixture(
            &tmp.path().join("missing-pkg/package.json"),
            r#"{"name":"missing-pkg-app","type":"module"}"#,
        );
        write_fixture(
            &tmp.path().join("missing-pkg/entry.ts"),
            r#"import "missing-package";"#,
        );
        let missing = run_check(
            &tmp.path().join("missing-pkg/entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .unwrap_err();
        assert_error_snapshot(
            &missing,
            "missing package",
            "compile",
            "cannot resolve `missing-package`",
        );

        write_fixture(
            &tmp.path().join("bad-export/package.json"),
            r#"{"name":"bad-export-app","type":"module","dependencies":{"bad-export":"file:packages/bad-export"}}"#,
        );
        write_fixture(
            &tmp.path().join("bad-export/entry.ts"),
            r#"import "bad-export";"#,
        );
        write_fixture(
            &tmp.path()
                .join("bad-export/packages/bad-export/package.json"),
            r#"{"name":"bad-export","type":"module","exports":"dist/index.js"}"#,
        );
        write_fixture(
            &tmp.path()
                .join("bad-export/packages/bad-export/dist/index.js"),
            "export const value = 1;\n",
        );
        let bad_export = run_check(
            &tmp.path().join("bad-export/entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .unwrap_err();
        assert_error_snapshot(&bad_export, "bad export", "compile", "must start with `./`");

        write_fixture(
            &tmp.path().join("condition-miss/package.json"),
            r#"{"name":"condition-miss-app","type":"module","dependencies":{"conditioned":"file:packages/conditioned"}}"#,
        );
        write_fixture(
            &tmp.path().join("condition-miss/entry.ts"),
            r#"import "conditioned";"#,
        );
        write_fixture(
            &tmp.path()
                .join("condition-miss/packages/conditioned/package.json"),
            r#"{"name":"conditioned","type":"module","exports":{".":{"browser":"./browser.js"}}}"#,
        );
        write_fixture(
            &tmp.path()
                .join("condition-miss/packages/conditioned/browser.js"),
            "export const value = 1;\n",
        );
        let condition_miss = run_check(
            &tmp.path().join("condition-miss/entry.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .unwrap_err();
        assert_error_snapshot(
            &condition_miss,
            "condition miss",
            "compile",
            "no matching export condition",
        );

        write_fixture(&tmp.path().join("syntax.ts"), "function {\n");
        let syntax = run_check(
            &tmp.path().join("syntax.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .unwrap_err();
        assert_error_snapshot(&syntax, "syntax error", "compile", "compile failed");

        write_fixture(&tmp.path().join("compile.ts"), "enum E { A }\n");
        let compile = run_check(
            &tmp.path().join("compile.ts"),
            false,
            &CapabilitySet::default(),
        )
        .await
        .unwrap_err();
        assert_error_snapshot(&compile, "compile error", "compile", "TSEnumDeclaration");

        write_fixture(&tmp.path().join("runtime.ts"), "throw 'boom';\n");
        let runtime = run_file(
            &tmp.path().join("runtime.ts"),
            &[],
            false,
            None,
            &CapabilitySet::default(),
            &startup_timer,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert_error_snapshot(&runtime, "runtime throw", "runtime", "boom");

        let capability = OtterError::Capability {
            capability: "fs_read".to_string(),
            detail: Some("read blocked by snapshot test".to_string()),
        };
        assert_error_snapshot(&capability, "blocked capability", "capability", "fs_read");

        let install = run_pm_install(&tmp.path().join("install-failure"), false)
            .await
            .unwrap_err();
        assert_error_snapshot(
            &install,
            "package install failure",
            "config",
            "package.json",
        );
    }

    fn assert_error_snapshot(
        err: &OtterError,
        label: &str,
        expected_kind: &str,
        expected_text: &str,
    ) {
        let json = err.to_json().expect("error JSON");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse error JSON");
        assert_eq!(
            value["error"]["kind"], expected_kind,
            "{label} JSON kind changed: {json}"
        );
        let text = err.to_string();
        assert!(
            text.contains(expected_text) || json.contains(expected_text),
            "{label} diagnostic changed\ntext: {text}\njson: {json}"
        );
    }

    /// Recursively copy a committed fixture directory into a scratch
    /// root so a test can materialize gitignored state (e.g.
    /// `node_modules`) without touching the repository tree.
    fn copy_fixture_tree(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let target = dst.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_fixture_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    fn write_fixture(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crate lives under workspace/crates/otter-cli")
            .to_path_buf()
    }
}
