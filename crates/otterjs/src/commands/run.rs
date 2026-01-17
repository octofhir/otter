//! Run command - execute a JavaScript/TypeScript file.

use anyhow::Result;
use clap::Args;
use otter_engine::{
    Capabilities, CapabilitiesBuilder, EnvStoreBuilder, IsolatedEnvStore, LoaderConfig,
    ModuleGraph, ModuleLoader, parse_env_file, parse_imports,
};
use otter_node::{
    ActiveNetServerCount, ActiveServerCount, ProcessInfo, ext, has_ipc, init_net_manager,
    IpcChannel, NetEvent,
};
use otter_runtime::HttpEvent;
use otter_runtime::{
    JscConfig, JscRuntime, bundle_modules, needs_transpilation, set_net_permission_checker,
    set_tokio_handle, transpile_typescript,
    tsgo::{TypeCheckConfig, check_types, format_diagnostics, has_errors},
};
use otter_jsc_sys::{
    JSContextGetGlobalObject, JSContextRef, JSObjectCallAsFunction, JSObjectGetProperty,
    JSObjectIsFunction, JSObjectRef, JSStringCreateWithUTF8CString, JSStringRelease,
    JSValueMakeNumber, JSValueProtect, JSValueRef,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
    /// File to execute
    pub entry: PathBuf,

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
}

impl RunCommand {
    pub async fn run(&self, config: &Config) -> Result<()> {
        if self.watch {
            self.run_watch_mode(config).await
        } else {
            self.run_once(config).await
        }
    }

    async fn run_watch_mode(&self, config: &Config) -> Result<()> {
        println!("\x1b[36m[HMR]\x1b[0m Watching for file changes...");

        let watch_config = WatchConfig::default();
        let mut watcher = FileWatcher::new(watch_config.clone());

        // Watch the directory containing the entry file
        let watch_dir = self
            .entry
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        watcher
            .watch(&watch_dir)
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        // Initial run
        clear_console(watch_config.clear_console);
        let result = self.run_once(config).await;
        if let Err(e) = result {
            eprintln!("\x1b[31m[Error]\x1b[0m {}", e);
        }

        // Watch loop
        loop {
            match watcher.wait_for_change() {
                Some(WatchEvent::FilesChanged(files)) => {
                    print_reload_message(&files);
                    clear_console(watch_config.clear_console);

                    let result = self.run_once(config).await;
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

    async fn run_once(&self, config: &Config) -> Result<()> {
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
        let is_typescript = needs_transpilation(&self.entry.to_string_lossy());
        let type_check_enabled = self.check || config.typescript.check;
        if type_check_enabled && is_typescript {
            self.type_check().await?;
            timing!("type_check");
        }

        // Read source
        let source = std::fs::read_to_string(&self.entry)?;
        timing!("source_read");

        // Build module loader config from CLI config
        let loader_config = build_loader_config(&self.entry, &config.modules);
        tracing::debug!(
            base_dir = %loader_config.base_dir.display(),
            remote_allowlist_count = loader_config.remote_allowlist.len(),
            import_map_count = loader_config.import_map.len(),
            "Module loader config"
        );

        // Check if file has imports - if so, use module bundler
        let imports = parse_imports(&source);
        let code = if !imports.is_empty() {
            tracing::debug!(imports_count = imports.len(), "Loading module dependencies");

            // Create module loader and graph
            let loader = Arc::new(ModuleLoader::new(loader_config));
            let mut graph = ModuleGraph::new(loader.clone());

            // Load entry file as file:// URL
            let entry_url = format!("file://{}", self.entry.canonicalize()?.display());
            graph.load(&entry_url).await?;

            // Bundle all modules in topological order
            let execution_order = graph.execution_order();
            let mut modules_for_bundle: Vec<(&str, &str, HashMap<String, String>)> = Vec::new();

            for url in &execution_order {
                if let Some(node) = graph.get(url) {
                    // Build dependency map for this module
                    let mut deps = HashMap::new();
                    for dep_specifier in &node.dependencies {
                        if let Ok(resolved) = loader.resolve(dep_specifier, Some(url)) {
                            deps.insert(dep_specifier.clone(), resolved);
                        }
                    }
                    modules_for_bundle.push((url, node.executable_source(), deps));
                }
            }

            // Convert to borrowed refs for bundle_modules
            let modules_refs: Vec<(&str, &str, &HashMap<String, String>)> = modules_for_bundle
                .iter()
                .map(|(url, src, deps)| (*url, *src, deps))
                .collect();

            bundle_modules(modules_refs)
        } else {
            // No imports - just transpile if needed
            if is_typescript {
                let result = transpile_typescript(&source)
                    .map_err(|e| anyhow::anyhow!("Transpilation error: {}", e))?;
                timing!("transpile");
                result.code
            } else {
                source
            }
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
        let active_net_server_count = init_net_manager(net_event_tx);

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
        runtime.register_extension(ext::crypto())?;
        timing!("ext_crypto");
        runtime.register_extension(ext::util())?;
        timing!("ext_util");
        runtime.register_extension(ext::process())?;
        timing!("ext_process");
        runtime.register_extension(ext::os())?;
        timing!("ext_os");
        runtime.register_extension(ext::child_process())?;
        timing!("ext_child_process");

        // Register net extension for node:net (TCP server/socket)
        runtime.register_extension(ext::net())?;
        timing!("ext_net");

        // Register HTTP server extension for Otter.serve()
        let (http_server_ext, active_http_server_count) = ext::http_server(http_event_tx);
        runtime.register_extension(http_server_ext)?;
        timing!("ext_http_server");

        // Register http extension for node:http (built on Otter.serve())
        // Must be loaded AFTER http_server_extension since it depends on Otter.serve()
        runtime.register_extension(ext::http())?;
        timing!("ext_http");

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
                .chain(std::iter::once(self.entry.to_string_lossy().to_string()))
                .chain(self.args.iter().cloned())
                .collect(),
        );

        // Set up Otter global namespace with args and capabilities
        let args_json = serde_json::to_string(&self.args)?;
        let hmr_code = if self.watch { hmr_runtime_code() } else { "" };
        let process_setup = process_info.to_js_setup();
        let setup = format!(
            "{hmr_code}\n\
             {process_setup}\n\
             globalThis.Otter = globalThis.Otter || {{}};\n\
             globalThis.Otter.args = {};\n\
             globalThis.Otter.capabilities = {};\n",
            args_json,
            serde_json::to_string(&caps_to_json(&caps))?,
        );

        // Execute with error handling wrapper
        let wrapped = format!(
            "{setup}\n\
             globalThis.__otter_script_error = null;\n\
             (async () => {{\n\
               try {{\n\
                 {code}\n\
               }} catch (err) {{\n\
                 globalThis.__otter_script_error = err ? String(err) : 'Error';\n\
               }}\n\
             }})();\n",
        );

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
            timeout,
        ).await?;
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
            let content = std::fs::read_to_string(env_file)
                .map_err(|e| anyhow::anyhow!("Failed to read env file '{}': {}", env_file.display(), e))?;
            let vars = parse_env_file(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse env file '{}': {}", env_file.display(), e))?;
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
            let all_vars: Vec<&str> = config.permissions.allow_env
                .iter()
                .flat_map(|s| s.split(','))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            builder = builder.passthrough(&all_vars);
        }

        Ok(builder.build())
    }

    async fn type_check(&self) -> Result<()> {
        let tsconfig = crate::config::find_tsconfig_for_file(&self.entry);

        let config = TypeCheckConfig {
            enabled: true,
            tsconfig,
            strict: true,
            skip_lib_check: true,
            target: None,
            module: None,
            lib: vec!["ES2022".to_string(), "DOM".to_string()],
        };

        let diagnostics = check_types(std::slice::from_ref(&self.entry), &config)
            .await
            .map_err(|e| anyhow::anyhow!("Type check failed: {}", e))?;

        if has_errors(&diagnostics) {
            eprint!("{}", format_diagnostics(&diagnostics));
            std::process::exit(1);
        }

        Ok(())
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

/// Build LoaderConfig from CLI entry file and ModulesConfig
///
/// This wires the CLI configuration to the module loader.
fn build_loader_config(entry: &Path, modules: &ModulesConfig) -> LoaderConfig {
    let base_dir = entry
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

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
    }
}

/// Run the event loop with HTTP and net event handling.
///
/// High-performance event loop that processes events with minimal latency.
/// Uses try_recv() for non-blocking poll, yield_now() to allow I/O processing.
/// Process stays alive while any HTTP or net servers are active.
async fn run_event_loop_with_events(
    runtime: &JscRuntime,
    mut http_event_rx: UnboundedReceiver<HttpEvent>,
    mut net_event_rx: UnboundedReceiver<NetEvent>,
    active_http_servers: ActiveServerCount,
    active_net_servers: ActiveNetServerCount,
    timeout: Duration,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    let start = Instant::now();
    let has_timeout = timeout != Duration::ZERO;
    let ctx = runtime.context().raw();

    let mut idle_cycles = 0u32;
    // For scripts without active servers, exit immediately after becoming idle.
    // For servers, use longer idle threshold to avoid premature exit.
    const MAX_IDLE_CYCLES_SCRIPT: u32 = 1;
    const MAX_IDLE_CYCLES_SERVER: u32 = 100;

    loop {
        let has_active_http = active_http_servers.load(Ordering::Relaxed) > 0;
        let has_active_net = active_net_servers.load(Ordering::Relaxed) > 0;
        let has_active_servers = has_active_http || has_active_net;

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
        let _ = runtime.poll_event_loop();

        // Yield to tokio to allow I/O to flow (hyper needs this!)
        if has_active_servers || processed > 0 || runtime.has_pending_tasks() {
            idle_cycles = 0;
            tokio::task::yield_now().await;
        } else {
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
