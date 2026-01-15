//! Run command - execute a JavaScript/TypeScript file.

use anyhow::Result;
use clap::Args;
use otter_engine::{
    Capabilities, CapabilitiesBuilder, LoaderConfig, ModuleGraph, ModuleLoader, parse_imports,
};
use otter_node::{
    create_buffer_extension, create_fs_extension, create_path_extension, create_test_extension,
};
use otter_runtime::{
    JscConfig, JscRuntime, bundle_modules, needs_transpilation, set_net_permission_checker,
    transpile_typescript,
    tsgo::{TypeCheckConfig, check_types, format_diagnostics, has_errors},
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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

    /// Allow subprocess execution
    #[arg(long = "allow-run")]
    pub allow_run: bool,

    /// Allow all permissions
    #[arg(long = "allow-all", short = 'A')]
    pub allow_all: bool,

    /// Skip type checking
    #[arg(long = "no-check")]
    pub no_check: bool,

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
        // Build capabilities from CLI flags + config
        let caps = self.build_capabilities(config);

        // Set network permission checker for fetch() API using capabilities
        let caps_for_net = Arc::new(caps.clone());
        set_net_permission_checker(Box::new(move |host| caps_for_net.can_net(host)));

        // Type check if needed and file is TypeScript
        let is_typescript = needs_transpilation(&self.entry.to_string_lossy());
        if !self.no_check && is_typescript && config.typescript.check {
            self.type_check().await?;
        }

        // Read source
        let source = std::fs::read_to_string(&self.entry)?;

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
                result.code
            } else {
                source
            }
        };

        // Create runtime with capabilities
        let runtime = JscRuntime::new(JscConfig::default())?;

        // Register Node.js compatibility extensions
        runtime.register_extension(create_path_extension())?;
        runtime.register_extension(create_buffer_extension())?;
        runtime.register_extension(create_fs_extension(caps.clone()))?;
        runtime.register_extension(create_test_extension())?;

        // Set up Otter global namespace with args and capabilities
        let args_json = serde_json::to_string(&self.args)?;
        let hmr_code = if self.watch { hmr_runtime_code() } else { "" };
        let setup = format!(
            "{hmr_code}\n\
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

        let timeout = if self.timeout == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(self.timeout)
        };

        runtime.run_event_loop_until_idle(timeout)?;

        // Check for script errors
        let error = runtime.context().get_global("__otter_script_error")?;
        if !error.is_null() && !error.is_undefined() {
            return Err(anyhow::anyhow!("{}", error.to_string()?));
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
