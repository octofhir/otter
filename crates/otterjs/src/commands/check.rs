//! Check command - type check TypeScript files.

use anyhow::Result;
use clap::Args;
use otter_runtime::tsgo::{TypeCheckConfig, check_types, format_diagnostics, has_errors};
use std::path::PathBuf;

use crate::config::Config;

#[derive(Args)]
pub struct CheckCommand {
    /// Files to type check
    #[arg(required = true)]
    pub files: Vec<PathBuf>,

    /// Path to tsconfig.json
    #[arg(long, short = 'p')]
    pub project: Option<PathBuf>,

    /// Disable strict mode
    #[arg(long)]
    pub no_strict: bool,

    /// Only show errors, not warnings
    #[arg(long)]
    pub quiet: bool,
}

impl CheckCommand {
    pub async fn run(&self, config: &Config) -> Result<()> {
        let file_count = self.files.len();
        if !self.quiet {
            println!("Type checking {} file(s)...", file_count);
        }

        // Determine tsconfig: explicit flag > auto-discovery > config file > none
        let tsconfig = self
            .project
            .clone()
            .or_else(|| {
                self.files
                    .first()
                    .and_then(|f| crate::config::find_tsconfig_for_file(f))
            })
            .or_else(|| config.typescript.tsconfig.clone());

        let strict = !self.no_strict && config.typescript.strict;

        let type_config = TypeCheckConfig {
            enabled: true,
            tsconfig,
            strict,
            skip_lib_check: true,
            target: None,
            module: None,
            lib: vec!["ES2022".to_string(), "DOM".to_string()],
        };

        let diagnostics = check_types(&self.files, &type_config)
            .await
            .map_err(|e| anyhow::anyhow!("Type check failed: {}", e))?;

        if diagnostics.is_empty() {
            if !self.quiet {
                println!("No type errors found.");
            }
            return Ok(());
        }

        eprint!("{}", format_diagnostics(&diagnostics));

        let error_count = diagnostics.iter().filter(|d| d.is_error()).count();
        let warning_count = diagnostics.len() - error_count;

        if has_errors(&diagnostics) {
            Err(anyhow::anyhow!(
                "Found {} error(s) and {} warning(s)",
                error_count,
                warning_count
            ))
        } else {
            if !self.quiet {
                println!("Found {} warning(s)", warning_count);
            }
            Ok(())
        }
    }
}
