//! Run command - execute a JavaScript/TypeScript file.

use anyhow::Result;
use clap::Args;
use otter_engine::{Capabilities, CapabilitiesBuilder};
use otter_runtime::{
    JscConfig, JscRuntime, needs_transpilation, transpile_typescript,
    tsgo::{TypeCheckConfig, check_types, format_diagnostics, has_errors},
};
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Config;

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
        // Build capabilities from CLI flags + config
        let caps = self.build_capabilities(config);

        // Type check if needed and file is TypeScript
        let is_typescript = needs_transpilation(&self.entry.to_string_lossy());
        if !self.no_check && is_typescript && config.typescript.check {
            self.type_check().await?;
        }

        // Read source
        let source = std::fs::read_to_string(&self.entry)?;

        // Transpile TypeScript if needed
        let code = if is_typescript {
            let result = transpile_typescript(&source)
                .map_err(|e| anyhow::anyhow!("Transpilation error: {}", e))?;
            result.code
        } else {
            source
        };

        // Create runtime with capabilities
        let runtime = JscRuntime::new(JscConfig::default())?;

        // Set up Otter global namespace with args and capabilities
        let args_json = serde_json::to_string(&self.args)?;
        let setup = format!(
            "globalThis.Otter = globalThis.Otter || {{}};\n\
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
        // Auto-discover tsconfig from entry file's directory (Bun-style)
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
