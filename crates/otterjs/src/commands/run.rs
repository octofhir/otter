//! Run command - execute a JavaScript/TypeScript file or package.json script.

use anyhow::Result;
use clap::Args;
use otter_engine::{
    Capabilities, CapabilitiesBuilder, EnvStoreBuilder, IsolatedEnvStore, LoaderConfig,
    ModuleGraph, ModuleLoader, dynamic_import, parse_env_file, parse_imports,
};
use otter_jsc_sys::{
    JSContextGetGlobalObject, JSContextRef, JSObjectCallAsFunction, JSObjectGetProperty,
    JSObjectIsFunction, JSObjectRef, JSStringCreateWithUTF8CString, JSStringRelease,
    JSValueMakeNumber, JSValueProtect, JSValueRef,
};
use otter_kv::kv_extension;
use otter_node::{
    ActiveNetServerCount, ActiveServerCount, ActiveWorkerCount, IpcChannel, NetEvent, ProcessInfo,
    ext, has_ipc, init_net_manager,
};
use otter_pm::{ScriptRunner, format_scripts_list};
use otter_runtime::HttpEvent;
use otter_runtime::{
    JscConfig, JscRuntime, ModuleFormat, ModuleInfo, bundle_modules_mixed, needs_transpilation,
    set_net_permission_checker, set_tokio_handle, transpile_typescript,
    tsgo::{TypeCheckConfig, check_types, format_diagnostics, has_errors},
};
use otter_sql::sql_extension;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use strsim::levenshtein;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Cached HTTP dispatch function for fast handler invocation
struct CachedDispatchFn {
    ctx: JSContextRef,
    func: JSObjectRef,
}

thread_local! {
    /// Thread-local cache for the HTTP dispatch function
    static CACHED_DISPATCH_FN: RefCell<Option<CachedDispatchFn>> = const { RefCell::new(None) };
}

use crate::config::{Config, ModulesConfig};
use crate::watch::{
    FileWatcher, WatchConfig, WatchEvent, clear_console, hmr_runtime_code, print_reload_message,
};

#[derive(Args)]
pub struct RunCommand {
    /// File to execute OR script name from package.json
    pub entry: Option<String>,

    /// Arguments to pass to script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// Allow file system read access (use without value for all paths)
    #[arg(long = "allow-read", value_name = "PATH", num_args = 0..)]
    pub allow_read: Option<Vec<String>>,

    /// Allow file system write access (use without value for all paths)
    #[arg(long = "allow-write", value_name = "PATH", num_args = 0..)]
    pub allow_write: Option<Vec<String>>,

    /// Allow network access (use without value for all hosts)
    #[arg(long = "allow-net", value_name = "HOST", num_args = 0..)]
    pub allow_net: Option<Vec<String>>,

    /// Allow environment variable access (use without value for all vars)
    #[arg(long = "allow-env", value_name = "VAR", num_args = 0..)]
    pub allow_env: Option<Vec<String>>,

    /// Load environment variables from file(s)
    #[arg(long = "env-file", value_name = "FILE")]
    pub env_files: Vec<PathBuf>,

    /// Set explicit environment variable (can be used multiple times)
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env_vars: Vec<String>,

    /// Allow subprocess execution
    #[arg(long = "allow-run")]
    pub allow_run: bool,

    /// Allow all permissions
    #[arg(long = "allow-all", short = 'A')]
    pub allow_all: bool,

    /// Enable type checking
    #[arg(long = "check")]
    pub check: bool,

    /// Timeout in milliseconds (0 = no timeout)
    #[arg(long, default_value_t = 30000)]
    pub timeout: u64,

    /// Watch mode - restart on file changes
    #[arg(long)]
    pub watch: bool,

    /// Force file execution even if script exists with same name
    #[arg(long)]
    pub file: bool,
}

impl RunCommand {
    pub async fn run(&self, config: &Config) -> Result<()> {
        let cwd = std::env::current_dir()?;

        // No entry provided - list available scripts
        let Some(ref entry) = self.entry else {
            return self.list_scripts(&cwd);
        };

        // Check if entry looks like a file path
        let path = PathBuf::from(entry);
        let is_file_path = self.file  // --file flag forces file mode
            || path.extension().is_some()  // has extension like .ts, .js
            || entry.starts_with(".")       // relative path like ./script.ts
            || entry.starts_with("/")       // absolute path
            || entry.contains('/'); // path with directory

        if is_file_path {
            // Treat as file
            if !path.exists() {
                return Err(self.file_not_found_error(&path, &cwd));
            }
            return self.run_file(&path, config).await;
        }

        // Try to find as package.json script
        if let Some(runner) = ScriptRunner::try_new(&cwd) {
            if runner.has_script(entry) {
                return self.run_script(&runner, entry);
            }

            // Script not found - check for typos and show suggestions
            let suggestions = runner.suggest(entry);
            if !suggestions.is_empty() {
                let mut msg = format!("error: Script '{}' not found in package.json\n\n", entry);
                msg.push_str("Did you mean?\n");
                for (name, cmd) in &suggestions {
                    let truncated = if cmd.len() > 40 {
                        format!("{}...", &cmd[..37])
                    } else {
                        cmd.to_string()
                    };
                    msg.push_str(&format!("  {} → {}\n", name, truncated));
                }
                msg.push_str(&format!(
                    "\nAvailable scripts: {}",
                    runner.script_names().join(", ")
                ));
                return Err(anyhow::anyhow!("{}", msg));
            }
        }

        // Check if file exists (maybe user forgot extension)
        if path.exists() {
            return self.run_file(&path, config).await;
        }

        // Neither script nor file - show helpful error
        Err(self.entry_not_found_error(entry, &cwd))
    }

    /// List available scripts from package.json
    fn list_scripts(&self, cwd: &Path) -> Result<()> {
        match ScriptRunner::try_new(cwd) {
            Some(runner) => {
                let scripts = runner.list();
                if scripts.is_empty() {
                    println!("No scripts defined in package.json");
                } else {
                    println!("Available scripts:\n");
                    println!("{}", format_scripts_list(&scripts));
                    println!("\nRun a script with: otter run <script-name>");
                }
                Ok(())
            }
            None => {
                println!("No package.json found in current directory.\n");
                println!("Usage: otter run <file.ts|script-name> [args...]");
                println!("\nExamples:");
                println!("  otter run app.ts          Run a TypeScript file");
                println!("  otter run build           Run 'build' script from package.json");
                println!("  otter run test -- --watch Pass args to script");
                Ok(())
            }
        }
    }

    /// Run a package.json script
    fn run_script(&self, runner: &ScriptRunner, name: &str) -> Result<()> {
        let script_cmd = runner
            .get_script(name)
            .ok_or_else(|| anyhow::anyhow!("Script '{}' not found", name))?;

        // Build the command with arguments
        let full_cmd = if self.args.is_empty() {
            script_cmd.clone()
        } else {
            format!("{} {}", script_cmd, shell_escape_args(&self.args))
        };

        // Build PATH with node_modules/.bin
        let bin_path = runner.project_dir().join("node_modules/.bin");
        let path = build_script_path(&bin_path);

        let status = Command::new("sh")
            .arg("-c")
            .arg(&full_cmd)
            .current_dir(runner.project_dir())
            .env("PATH", &path)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;

        if !status.success() {
            let code = status.code().unwrap_or(1);
            std::process::exit(code);
        }

        Ok(())
    }

    /// Run a file (the original behavior)
    async fn run_file(&self, path: &Path, config: &Config) -> Result<()> {
        // Store path for internal use
        let entry_path = path.to_path_buf();

        if self.watch {
            self.run_watch_mode_file(&entry_path, config).await
        } else {
            self.run_once_file(&entry_path, config).await
        }
    }

    /// Error when file not found
    fn file_not_found_error(&self, path: &Path, cwd: &Path) -> anyhow::Error {
        let mut msg = format!("error: Cannot find file '{}'\n", path.display());

        // Try to find similar files
        if let Some(similar) = find_similar_files(path, cwd) {
            msg.push_str("\nSimilar files found:\n");
            for f in similar.iter().take(3) {
                msg.push_str(&format!("  {}\n", f));
            }
        }

        // Suggest scripts if available
        if let Some(runner) = ScriptRunner::try_new(cwd) {
            let names = runner.script_names();
            if !names.is_empty() {
                msg.push_str("\nDid you mean to run a script? Available scripts:\n  ");
                msg.push_str(&names.join(", "));
                msg.push('\n');
            }
        }

        anyhow::anyhow!("{}", msg)
    }

    /// Error when neither file nor script found
    fn entry_not_found_error(&self, entry: &str, cwd: &Path) -> anyhow::Error {
        let mut msg = format!("error: '{}' is not a file or script\n", entry);

        // Check for similar files
        let path = PathBuf::from(entry);
        if let Some(similar) = find_similar_files(&path, cwd) {
            msg.push_str("\nSimilar files found:\n");
            for f in similar.iter().take(3) {
                msg.push_str(&format!("  {}\n", f));
            }
        }

        // List available scripts
        if let Some(runner) = ScriptRunner::try_new(cwd) {
            let names = runner.script_names();
            if !names.is_empty() {
                msg.push_str("\nAvailable scripts:\n  ");
                msg.push_str(&names.join(", "));
                msg.push('\n');
            }
        }

        msg.push_str("\nUsage: otter run <file.ts> or otter run <script-name>");

        anyhow::anyhow!("{}", msg)
    }

    async fn run_watch_mode_file(&self, entry: &Path, config: &Config) -> Result<()> {
        println!("\x1b[36m[HMR]\x1b[0m Watching for file changes...");

        let watch_config = WatchConfig::default();
        let mut watcher = FileWatcher::new(watch_config.clone());

        // Watch the directory containing the entry file
        let watch_dir = entry
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        watcher
            .watch(&watch_dir)
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        // Initial run
        clear_console(watch_config.clear_console);
        let result = self.run_once_file(entry, config).await;
        if let Err(e) = result {
            eprintln!("\x1b[31m[Error]\x1b[0m {}", e);
        }

        // Watch loop
        loop {
            match watcher.wait_for_change() {
                Some(WatchEvent::FilesChanged(files)) => {
                    print_reload_message(&files);
                    clear_console(watch_config.clear_console);

                    let result = self.run_once_file(entry, config).await;
                    if let Err(e) = result {
                        eprintln!("\x1b[31m[Error]\x1b[0m {}", e);
                    }
                }
                Some(WatchEvent::Error(e)) => {
                    eprintln!("\x1b[33m[Watch Error]\x1b[0m {}", e);
                }
                None => {
                    // Channel closed, exit
                    break;
                }
            }
        }

        Ok(())
    }

    async fn run_once_file(&self, entry: &Path, config: &Config) -> Result<()> {
        let total_start = std::time::Instant::now();
        let mut phase_start = total_start;

        macro_rules! timing {
            ($name:expr) => {
                if std::env::var("OTTER_TIMING").is_ok() {
                    eprintln!("[TIMING] {}: {:?}", $name, phase_start.elapsed());
                    phase_start = std::time::Instant::now();
                }
            };
        }

        // Set tokio handle for async operations in extensions (HTTP server, etc.)
        set_tokio_handle(tokio::runtime::Handle::current());
        timing!("tokio_handle_set");

        // Build capabilities from CLI flags + config
        let caps = self.build_capabilities(config);
        timing!("capabilities_built");

        // Set network permission checker for fetch() API using capabilities
        let caps_for_net = Arc::new(caps.clone());
        set_net_permission_checker(Box::new(move |host| caps_for_net.can_net(host)));
        timing!("net_permission_set");

        // Type check if needed and file is TypeScript
        let is_typescript = needs_transpilation(&entry.to_string_lossy());
        let type_check_enabled = self.check || config.typescript.check;
        if type_check_enabled && is_typescript {
            self.type_check_file(entry).await?;
            timing!("type_check");
        }

        // Read source and strip shebang if present
        let source = std::fs::read_to_string(entry)?;
        let source = strip_shebang(&source);
        timing!("source_read");

        // Build module loader config from CLI config
        let loader_config = build_loader_config(entry, &config.modules);
        tracing::debug!(
            base_dir = %loader_config.base_dir.display(),
            remote_allowlist_count = loader_config.remote_allowlist.len(),
            import_map_count = loader_config.import_map.len(),
            "Module loader config"
        );

        // Clone loader_config for dynamic import extension (registered later)
        let loader_config_for_dynamic_import = loader_config.clone();

        // Check if file has imports - if so, use module bundler
        let imports = parse_imports(&source);
        let code = if !imports.is_empty() {
            tracing::debug!(imports_count = imports.len(), "Loading module dependencies");

            // Create module loader and graph
            let loader = Arc::new(ModuleLoader::new(loader_config));
            let mut graph = ModuleGraph::new(loader.clone());

            // Load entry file as file:// URL
            let entry_url = format!("file://{}", entry.canonicalize()?.display());
            graph.load(&entry_url).await?;

            // Bundle all modules in topological order with CJS/ESM detection
            let execution_order = graph.execution_order();
            let mut modules_for_bundle: Vec<(
                String,
                String,
                HashMap<String, String>,
                bool,
                Option<String>,
                Option<String>,
            )> = Vec::new();

            for url in &execution_order {
                // Skip built-in modules (node: and otter:) - they have no source
                // and are resolved via __otter_get_node_builtin
                if url.starts_with("node:") || url.starts_with("otter:") {
                    continue;
                }
                if let Some(node) = graph.get(url) {
                    // Build dependency map for this module
                    let mut deps = HashMap::new();
                    for record in &node.import_records {
                        if let Some(resolved) = record.resolved_url.clone() {
                            deps.insert(record.specifier.clone(), resolved);
                        }
                    }
                    let is_cjs = node.is_commonjs();
                    let dirname = node.dirname().map(|s| s.to_string());
                    let filename = node.filename().map(|s| s.to_string());
                    modules_for_bundle.push((
                        url.to_string(),
                        node.executable_source().to_string(),
                        deps,
                        is_cjs,
                        dirname,
                        filename,
                    ));
                }
            }

            // Convert to ModuleInfo for mixed bundling
            let modules: Vec<ModuleInfo<'_>> = modules_for_bundle
                .iter()
                .map(|(url, src, deps, is_cjs, dirname, filename)| ModuleInfo {
                    url: url.as_str(),
                    source: src.as_str(),
                    dependencies: deps,
                    format: if *is_cjs {
                        ModuleFormat::CommonJS
                    } else {
                        ModuleFormat::ESM
                    },
                    dirname: dirname.as_deref(),
                    filename: filename.as_deref(),
                })
                .collect();

            bundle_modules_mixed(modules)
        } else {
            // No imports - just transpile if needed
            let code = if is_typescript {
                let result = transpile_typescript(&source)
                    .map_err(|e| anyhow::anyhow!("Transpilation error: {}", e))?;
                timing!("transpile");
                result.code
            } else {
                source
            };

            // Transform dynamic imports even without static imports
            // import(variableName) -> __otter_dynamic_import(variableName)
            otter_runtime::modules_ast::transform_dynamic_imports(&code).unwrap_or(code)
        };
        timing!("code_prepared");

        // Create runtime with capabilities
        let runtime = JscRuntime::new(JscConfig::default())?;
        timing!("jsc_runtime_created");

        // Create async HTTP event channel for Otter.serve() - instant wake on events!
        let (http_event_tx, http_event_rx) = unbounded_channel::<HttpEvent>();

        // Create net event channel for node:net TCP servers
        let (net_event_tx, net_event_rx) = unbounded_channel::<NetEvent>();

        // Initialize net manager (returns active server count for keep-alive)
        let active_net_server_count = init_net_manager(net_event_tx.clone());

        // Register Web API extensions (URL, etc.)
        runtime.register_extension(ext::url())?;
        timing!("ext_url");

        // Register Node.js compatibility extensions
        runtime.register_extension(ext::path())?;
        timing!("ext_path");
        runtime.register_extension(ext::buffer())?;
        timing!("ext_buffer");
        runtime.register_extension(ext::fs(caps.clone()))?;
        timing!("ext_fs");
        runtime.register_extension(ext::test())?;
        timing!("ext_test");
        runtime.register_extension(ext::events())?;
        timing!("ext_events");
        runtime.register_extension(ext::async_hooks())?;
        timing!("ext_async_hooks");
        runtime.register_extension(ext::crypto())?;
        timing!("ext_crypto");
        runtime.register_extension(ext::util())?;
        timing!("ext_util");
        runtime.register_extension(ext::process())?;
        timing!("ext_process");
        runtime.register_extension(ext::os())?;
        timing!("ext_os");
        runtime.register_extension(ext::perf_hooks())?;
        timing!("ext_perf_hooks");
        runtime.register_extension(ext::module())?;
        timing!("ext_module");
        runtime.register_extension(ext::child_process())?;
        timing!("ext_child_process");
        let (worker_threads_ext, active_worker_count) = ext::worker_threads();
        runtime.register_extension(worker_threads_ext)?;
        timing!("ext_worker_threads");
        runtime.register_extension(ext::string_decoder())?;
        timing!("ext_string_decoder");
        runtime.register_extension(ext::readline())?;
        timing!("ext_readline");
        runtime.register_extension(ext::node_stream())?;
        timing!("ext_node_stream");

        // Register additional Node.js compatibility extensions
        runtime.register_extension(ext::assert())?;
        timing!("ext_assert");
        runtime.register_extension(ext::zlib())?;
        timing!("ext_zlib");
        runtime.register_extension(ext::querystring())?;
        timing!("ext_querystring");
        // runtime.register_extension(ext::dgram())?;  // EventEmitter not available
        // runtime.register_extension(ext::dns())?;

        // Register net extension for node:net (TCP server/socket)
        runtime.register_extension(ext::net())?;
        timing!("ext_net");

        // Register TLS extension for node:tls (TLS client sockets)
        let (tls_ext, _tls_active_count) = ext::tls(net_event_tx.clone());
        runtime.register_extension(tls_ext)?;
        timing!("ext_tls");

        // Register HTTP server extension for Otter.serve()
        let (http_server_ext, active_http_server_count) = ext::http_server(http_event_tx);
        runtime.register_extension(http_server_ext)?;
        timing!("ext_http_server");

        // Register http extension for node:http (built on Otter.serve())
        // Must be loaded AFTER http_server_extension since it depends on Otter.serve()
        runtime.register_extension(ext::http())?;
        timing!("ext_http");
        runtime.register_extension(ext::https())?;
        timing!("ext_https");
        runtime.register_extension(ext::http2())?;
        timing!("ext_http2");
        runtime.register_extension(ext::tty())?;
        timing!("ext_tty");

        // Register SQL extension for "otter" module (sql, SQL)
        runtime.register_extension(sql_extension())?;
        timing!("ext_sql");

        // Register KV extension for "otter" module (kv)
        runtime.register_extension(kv_extension())?;
        timing!("ext_kv");

        // Register dynamic import extension for runtime module loading
        runtime.register_extension(dynamic_import::extension(loader_config_for_dynamic_import))?;
        timing!("ext_dynamic_import");

        // Check for IPC channel (forked child process)
        #[cfg(unix)]
        if has_ipc() {
            if let Some(fd) = otter_node::ipc::get_ipc_fd() {
                // SAFETY: fd is a valid Unix socket passed from parent
                if let Ok(ipc_channel) = unsafe { IpcChannel::from_raw_fd(fd) } {
                    runtime.register_extension(ext::process_ipc(ipc_channel))?;
                    tracing::debug!(fd, "IPC channel established with parent process");
                }
            }
        }

        // Build isolated env store from CLI flags
        let env_store = Arc::new(self.build_env_store(config)?);

        // Create process info with isolated env
        let process_info = ProcessInfo::new(
            env_store,
            std::iter::once("otter".to_string())
                .chain(std::iter::once(entry.to_string_lossy().to_string()))
                .chain(self.args.iter().cloned())
                .collect(),
        );

        // Set up Otter global namespace with args and capabilities
        let args_json = serde_json::to_string(&self.args)?;
        let hmr_code = if self.watch { hmr_runtime_code() } else { "" };
        let process_setup = process_info.to_js_setup();

        // Get entry dirname and filename for proper require() resolution
        let entry_canonical = entry.canonicalize()?;
        let entry_dirname = entry_canonical
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string());
        let entry_filename = entry_canonical.to_string_lossy().to_string();

        let setup = format!(
            "{hmr_code}\n\
             {process_setup}\n\
             globalThis.__otter_lock_builtins && globalThis.__otter_lock_builtins();\n\
             globalThis.__otter_set_entry_dirname && globalThis.__otter_set_entry_dirname({entry_dirname_json}, {entry_filename_json});\n\
             globalThis.Otter = globalThis.Otter || {{}};\n\
             globalThis.Otter.args = {args_json};\n\
             globalThis.Otter.capabilities = {caps_json};\n",
            entry_dirname_json = serde_json::to_string(&entry_dirname)?,
            entry_filename_json = serde_json::to_string(&entry_filename)?,
            args_json = args_json,
            caps_json = serde_json::to_string(&caps_to_json(&caps))?,
        );

        // Execute with error handling wrapper
        // NOTE: We store the main promise in a global and attach a .catch() handler.
        // This ensures JSC keeps tracking the Promise, preventing premature event loop exit.
        // Without this, calling an async function like `main()` without awaiting would
        // leave the Promise unobserved, and JSC might not keep the event loop alive.
        let wrapped = format!(
            "{setup}\n\
             globalThis.__otter_script_error = null;\n\
             globalThis.__otter_main_promise = (async () => {{\n\
               try {{\n\
                 {code}\n\
               }} catch (err) {{\n\
                 globalThis.__otter_script_error = err ? String(err) : 'Error';\n\
               }}\n\
             }})();\n\
             globalThis.__otter_main_promise.catch(() => {{}});\n",
        );

        // Write bundle for debugging if env var is set
        if std::env::var("OTTER_DEBUG_BUNDLE").is_ok() {
            std::fs::write("/tmp/otter_bundle_debug.js", &wrapped).ok();
        }
        runtime.eval(&wrapped)?;
        timing!("script_eval");

        let timeout = if self.timeout == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(self.timeout)
        };

        // Run event loop with HTTP and net event handling
        run_event_loop_with_events(
            &runtime,
            http_event_rx,
            net_event_rx,
            active_http_server_count,
            active_net_server_count,
            active_worker_count,
            timeout,
        )
        .await?;
        timing!("event_loop_done");

        // Check for script errors
        let error = runtime.context().get_global("__otter_script_error")?;
        if !error.is_null() && !error.is_undefined() {
            return Err(anyhow::anyhow!("{}", error.to_string()?));
        }

        if std::env::var("OTTER_TIMING").is_ok() {
            eprintln!("[TIMING] TOTAL: {:?}", total_start.elapsed());
        }

        Ok(())
    }

    fn build_capabilities(&self, config: &Config) -> Capabilities {
        if self.allow_all {
            return Capabilities::all();
        }

        let mut builder = CapabilitiesBuilder::new();

        // Merge CLI flags with config
        // CLI flags take precedence

        // Read permissions
        if let Some(ref paths) = self.allow_read {
            if paths.is_empty() {
                builder = builder.allow_read_all();
            } else {
                builder = builder.allow_read(paths.iter().map(PathBuf::from));
            }
        } else if !config.permissions.allow_read.is_empty() {
            builder = builder.allow_read(config.permissions.allow_read.iter().map(PathBuf::from));
        }

        // Write permissions
        if let Some(ref paths) = self.allow_write {
            if paths.is_empty() {
                builder = builder.allow_write_all();
            } else {
                builder = builder.allow_write(paths.iter().map(PathBuf::from));
            }
        } else if !config.permissions.allow_write.is_empty() {
            builder = builder.allow_write(config.permissions.allow_write.iter().map(PathBuf::from));
        }

        // Net permissions
        if let Some(ref hosts) = self.allow_net {
            if hosts.is_empty() {
                builder = builder.allow_net_all();
            } else {
                builder = builder.allow_net(hosts.iter().cloned());
            }
        } else if !config.permissions.allow_net.is_empty() {
            builder = builder.allow_net(config.permissions.allow_net.iter().cloned());
        }

        // Env permissions
        if let Some(ref vars) = self.allow_env {
            if vars.is_empty() {
                builder = builder.allow_env_all();
            } else {
                builder = builder.allow_env(vars.iter().cloned());
            }
        } else if !config.permissions.allow_env.is_empty() {
            builder = builder.allow_env(config.permissions.allow_env.iter().cloned());
        }

        // Subprocess
        if self.allow_run {
            builder = builder.allow_subprocess();
        }

        builder.build()
    }

    /// Build isolated environment store from CLI flags and config.
    ///
    /// Priority (highest to lowest):
    /// 1. Explicit `-e KEY=VALUE` flags
    /// 2. Variables from `--env-file` (later files override earlier)
    /// 3. Passthrough from `--allow-env` (filtered by deny patterns)
    fn build_env_store(&self, config: &Config) -> Result<IsolatedEnvStore> {
        let mut builder = EnvStoreBuilder::new();

        // Load from env files first (lowest priority of explicit vars)
        for env_file in &self.env_files {
            let content = std::fs::read_to_string(env_file).map_err(|e| {
                anyhow::anyhow!("Failed to read env file '{}': {}", env_file.display(), e)
            })?;
            let vars = parse_env_file(&content).map_err(|e| {
                anyhow::anyhow!("Failed to parse env file '{}': {}", env_file.display(), e)
            })?;
            builder = builder.explicit_vars(vars);
        }

        // Parse and add explicit -e KEY=VALUE vars (highest priority)
        for var in &self.env_vars {
            if let Some((key, value)) = var.split_once('=') {
                builder = builder.explicit(key.trim(), value);
            } else {
                // If no '=', treat as passthrough from host
                builder = builder.passthrough_var(var);
            }
        }

        // Add passthrough vars from --allow-env (filtered by deny patterns)
        if let Some(ref vars) = self.allow_env {
            if vars.is_empty() {
                // --allow-env without args: pass through ALL host vars (DANGEROUS!)
                // But still filtered by deny patterns
                for (key, _) in std::env::vars() {
                    builder = builder.passthrough_var(key);
                }
            } else {
                // Handle both comma-separated and multiple --allow-env flags
                let all_vars: Vec<&str> = vars
                    .iter()
                    .flat_map(|s| s.split(','))
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                builder = builder.passthrough(&all_vars);
            }
        } else if !config.permissions.allow_env.is_empty() {
            let all_vars: Vec<&str> = config
                .permissions
                .allow_env
                .iter()
                .flat_map(|s| s.split(','))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            builder = builder.passthrough(&all_vars);
        }

        Ok(builder.build())
    }

    async fn type_check_file(&self, entry: &Path) -> Result<()> {
        let tsconfig = crate::config::find_tsconfig_for_file(entry);

        let config = TypeCheckConfig {
            enabled: true,
            tsconfig,
            strict: true,
            skip_lib_check: true,
            target: None,
            module: None,
            lib: vec!["ES2022".to_string(), "DOM".to_string()],
        };

        let diagnostics = check_types(std::slice::from_ref(&entry.to_path_buf()), &config)
            .await
            .map_err(|e| anyhow::anyhow!("Type check failed: {}", e))?;

        if has_errors(&diagnostics) {
            eprint!("{}", format_diagnostics(&diagnostics));
            std::process::exit(1);
        }

        Ok(())
    }
}

/// Escape arguments for shell execution
fn shell_escape_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.contains(' ') || arg.contains('"') || arg.contains('\'') {
                format!("'{}'", arg.replace('\'', "'\\''"))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build PATH with node_modules/.bin prepended
fn build_script_path(bin_path: &Path) -> String {
    let mut paths = Vec::new();

    // Local node_modules/.bin (highest priority)
    if bin_path.exists() {
        paths.push(bin_path.display().to_string());
    }

    // Walk up directory tree for nested node_modules
    let mut current = bin_path.parent().and_then(|p| p.parent());
    while let Some(dir) = current {
        let bin = dir.join("node_modules/.bin");
        if bin.exists() {
            paths.push(bin.display().to_string());
        }
        current = dir.parent();
    }

    // Existing PATH
    if let Ok(existing) = std::env::var("PATH") {
        paths.push(existing);
    }

    paths.join(":")
}

/// Find similar files using fuzzy matching
fn find_similar_files(target: &Path, cwd: &Path) -> Option<Vec<String>> {
    let target_name = target.file_name()?.to_string_lossy();
    let target_str = target_name.as_ref();

    let mut similar = Vec::new();

    // Get the directory to search
    let search_dir = if target.is_absolute() {
        target.parent().unwrap_or(cwd)
    } else if let Some(parent) = target.parent() {
        if parent.as_os_str().is_empty() {
            cwd
        } else {
            &cwd.join(parent)
        }
    } else {
        cwd
    };

    if let Ok(entries) = std::fs::read_dir(search_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip hidden files and directories
            if name.starts_with('.') {
                continue;
            }

            // Check file extensions for JS/TS files
            let is_script = name.ends_with(".ts")
                || name.ends_with(".js")
                || name.ends_with(".tsx")
                || name.ends_with(".jsx")
                || name.ends_with(".mjs")
                || name.ends_with(".mts");

            if is_script {
                let distance = levenshtein(target_str, &name);
                if distance <= 3 {
                    let rel_path = if search_dir == cwd {
                        name
                    } else if let Ok(rel) = search_dir.strip_prefix(cwd) {
                        format!("{}/{}", rel.display(), name)
                    } else {
                        name
                    };
                    similar.push((distance, rel_path));
                }
            }
        }
    }

    if similar.is_empty() {
        None
    } else {
        similar.sort_by_key(|(d, _)| *d);
        Some(similar.into_iter().map(|(_, p)| p).collect())
    }
}

/// Convert capabilities to a JSON-serializable format for the runtime
fn caps_to_json(caps: &Capabilities) -> serde_json::Value {
    serde_json::json!({
        "read": caps.fs_read.is_some(),
        "write": caps.fs_write.is_some(),
        "net": caps.net.is_some(),
        "env": caps.env.is_some(),
        "run": caps.subprocess,
        "ffi": caps.ffi,
        "hrtime": caps.hrtime,
    })
}

/// Strip shebang line from source code if present.
///
/// Handles both `#!/path/to/interpreter` and `#! /path/to/interpreter` formats.
fn strip_shebang(source: &str) -> String {
    if source.starts_with("#!") {
        // Find the end of the first line
        if let Some(newline_pos) = source.find('\n') {
            // Replace shebang with empty line to preserve line numbers
            format!("{}{}", " ".repeat(newline_pos), &source[newline_pos..])
        } else {
            // Entire file is just a shebang
            String::new()
        }
    } else {
        source.to_string()
    }
}

/// Build LoaderConfig from CLI entry file and ModulesConfig
///
/// This wires the CLI configuration to the module loader.
fn build_loader_config(entry: &Path, modules: &ModulesConfig) -> LoaderConfig {
    // Canonicalize entry path to get absolute directory for module resolution
    let base_dir = entry
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| {
            // Fallback: use parent of entry path (may be relative)
            entry
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        });

    let default = LoaderConfig::default();

    LoaderConfig {
        base_dir,
        remote_allowlist: if modules.remote_allowlist.is_empty() {
            default.remote_allowlist
        } else {
            modules.remote_allowlist.clone()
        },
        cache_dir: modules.cache_dir.clone().unwrap_or(default.cache_dir),
        import_map: modules.import_map.clone(),
        extensions: default.extensions,
        condition_names: default.condition_names,
        esm_conditions: default.esm_conditions,
        cjs_conditions: default.cjs_conditions,
    }
}

/// Run the event loop with HTTP and net event handling.
///
/// High-performance event loop that processes events with minimal latency.
/// Uses try_recv() for non-blocking poll, yield_now() to allow I/O processing.
/// Process stays alive while any HTTP or net servers or workers are active.
async fn run_event_loop_with_events(
    runtime: &JscRuntime,
    mut http_event_rx: UnboundedReceiver<HttpEvent>,
    mut net_event_rx: UnboundedReceiver<NetEvent>,
    active_http_servers: ActiveServerCount,
    active_net_servers: ActiveNetServerCount,
    active_workers: ActiveWorkerCount,
    timeout: Duration,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    let start = Instant::now();
    let has_timeout = timeout != Duration::ZERO;
    let ctx = runtime.context().raw();

    let mut idle_cycles = 0u32;
    // Scripts need a few idle cycles to let Promise continuations run.
    // This accounts for cases where async work completes but the event loop
    // hasn't yet processed the continuation (e.g., after child process close events).
    // For servers/workers, use longer idle threshold to avoid premature exit.
    const MAX_IDLE_CYCLES_SCRIPT: u32 = 5;
    const MAX_IDLE_CYCLES_SERVER: u32 = 100;

    loop {
        let has_active_http = active_http_servers.load(Ordering::Relaxed) > 0;
        let has_active_net = active_net_servers.load(Ordering::Relaxed) > 0;
        let has_active_workers = active_workers.load(Ordering::Relaxed) > 0;
        let has_active_servers = has_active_http || has_active_net || has_active_workers;

        // Timeout check - but not for active servers (keep-alive)
        if has_timeout && !has_active_servers && start.elapsed() >= timeout {
            break;
        }

        // Process all pending HTTP events (non-blocking hot path)
        let mut processed = 0usize;
        while let Ok(ev) = http_event_rx.try_recv() {
            dispatch_http_event_fast(ctx, ev.server_id, ev.request_id);
            processed += 1;
        }

        // Process all pending net events
        while let Ok(ev) = net_event_rx.try_recv() {
            dispatch_net_event(ctx, &ev);
            processed += 1;
        }

        // Poll JavaScript event loop (promises, timers, async ops)
        // IMPORTANT: We must check the return value - if events were processed,
        // we need another iteration to run any Promise continuations that were scheduled.
        let polled = runtime.poll_event_loop().unwrap_or(0);

        // Yield to tokio to allow I/O to flow (hyper needs this!)
        if has_active_servers || processed > 0 || polled > 0 {
            idle_cycles = 0;
            tokio::task::yield_now().await;
            continue;
        }

        if runtime.has_pending_tasks() {
            idle_cycles = 0;
            let sleep_for = runtime.next_wake_delay().max(Duration::from_millis(1));
            tokio::time::sleep(sleep_for).await;
            continue;
        }

        // No work at all - idle countdown
        idle_cycles += 1;
        let max_cycles = if has_active_servers {
            MAX_IDLE_CYCLES_SERVER
        } else {
            MAX_IDLE_CYCLES_SCRIPT
        };
        if idle_cycles >= max_cycles {
            break;
        }
        // Shorter sleep for scripts, longer for servers
        let sleep_ms = if has_active_servers { 5 } else { 1 };
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
    }

    Ok(())
}

/// Fast HTTP event dispatch using cached JSC function call.
///
/// This avoids the overhead of `runtime.eval()` which involves string formatting
/// and JavaScript parsing. Instead, it looks up `__otter_http_dispatch` once,
/// caches the function reference, and calls it directly via JSC FFI.
fn dispatch_http_event_fast(ctx: JSContextRef, server_id: u64, request_id: u64) {
    unsafe {
        let global = JSContextGetGlobalObject(ctx);

        // Try to get cached function, or look it up and cache it
        let func = CACHED_DISPATCH_FN.with(|cache| {
            let mut cache = cache.borrow_mut();

            // Check if we have a cached function for this context
            if let Some(ref cached) = *cache {
                if cached.ctx == ctx {
                    return Some(cached.func);
                }
            }

            // Look up the function
            let func_name = CString::new("__otter_http_dispatch").unwrap();
            let func_name_ref = JSStringCreateWithUTF8CString(func_name.as_ptr());
            let mut exception: JSValueRef = std::ptr::null_mut();

            let func_value = JSObjectGetProperty(ctx, global, func_name_ref, &mut exception);
            JSStringRelease(func_name_ref);

            if exception.is_null() && JSObjectIsFunction(ctx, func_value as JSObjectRef) {
                let func = func_value as JSObjectRef;

                // Protect from GC and cache
                JSValueProtect(ctx, func as JSValueRef);
                *cache = Some(CachedDispatchFn { ctx, func });

                Some(func)
            } else {
                None
            }
        });

        let Some(func) = func else {
            tracing::warn!(server_id, request_id, "HTTP dispatch function not found");
            return;
        };

        // Create arguments: [serverId, requestId]
        let args = [
            JSValueMakeNumber(ctx, server_id as f64),
            JSValueMakeNumber(ctx, request_id as f64),
        ];

        // Call the cached function directly
        let mut call_exception: JSValueRef = std::ptr::null_mut();
        JSObjectCallAsFunction(ctx, func, global, 2, args.as_ptr(), &mut call_exception);

        if !call_exception.is_null() {
            tracing::warn!(server_id, request_id, "HTTP dispatch threw exception");
        }
    }
}

// Cached net dispatch function for fast event delivery
thread_local! {
    static CACHED_NET_DISPATCH_FN: RefCell<Option<CachedDispatchFn>> = const { RefCell::new(None) };
}

/// Dispatch a net event to JavaScript by calling __otter_net_dispatch(json).
fn dispatch_net_event(ctx: JSContextRef, event: &NetEvent) {
    use otter_jsc_sys::JSValueMakeString;

    // Serialize event to JSON
    let json = match serde_json::to_string(event) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize net event");
            return;
        }
    };

    unsafe {
        let global = JSContextGetGlobalObject(ctx);

        // Try to get cached function, or look it up and cache it
        let func = CACHED_NET_DISPATCH_FN.with(|cache| {
            let mut cache = cache.borrow_mut();

            // Check if we have a cached function for this context
            if let Some(ref cached) = *cache {
                if cached.ctx == ctx {
                    return Some(cached.func);
                }
            }

            // Look up the function
            let func_name = CString::new("__otter_net_dispatch").unwrap();
            let func_name_ref = JSStringCreateWithUTF8CString(func_name.as_ptr());
            let mut exception: JSValueRef = std::ptr::null_mut();

            let func_value = JSObjectGetProperty(ctx, global, func_name_ref, &mut exception);
            JSStringRelease(func_name_ref);

            if exception.is_null() && JSObjectIsFunction(ctx, func_value as JSObjectRef) {
                let func = func_value as JSObjectRef;

                // Protect from GC and cache
                JSValueProtect(ctx, func as JSValueRef);
                *cache = Some(CachedDispatchFn { ctx, func });

                Some(func)
            } else {
                None
            }
        });

        let Some(func) = func else {
            tracing::warn!("Net dispatch function not found");
            return;
        };

        // Create JSON string argument
        let json_cstr = CString::new(json.as_str()).unwrap();
        let json_ref = JSStringCreateWithUTF8CString(json_cstr.as_ptr());
        let json_value = JSValueMakeString(ctx, json_ref);
        JSStringRelease(json_ref);

        // Call the cached function with JSON string
        let args = [json_value];
        let mut call_exception: JSValueRef = std::ptr::null_mut();
        JSObjectCallAsFunction(ctx, func, global, 1, args.as_ptr(), &mut call_exception);

        if !call_exception.is_null() {
            tracing::warn!("Net dispatch threw exception");
        }
    }
}
