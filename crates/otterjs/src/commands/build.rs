//! Build command - bundle and compile JavaScript/TypeScript.
//!
//! Usage:
//! - `otter build app.ts -o bundle.js` - bundle to JavaScript
//! - `otter build app.ts --compile -o myapp` - compile to standalone executable
//! - `otter build app.ts --outdir dist` - bundle to directory

use anyhow::{Context, Result};
use clap::Args;
use otter_engine::{LoaderConfig, ModuleGraph, ModuleLoader, NodeApiProfile, parse_imports};
use otter_runtime::{
    ModuleFormat, ModuleInfo, bundle_modules_mixed, needs_transpilation, transpile_typescript,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::embedded;

#[derive(Args, Debug)]
pub struct BuildCommand {
    /// Entry point file(s)
    #[arg(required = true)]
    pub entrypoints: Vec<PathBuf>,

    /// Output file (use with single entrypoint or --compile)
    #[arg(short = 'o', long)]
    pub outfile: Option<PathBuf>,

    /// Output directory (for multiple entrypoints)
    #[arg(long)]
    pub outdir: Option<PathBuf>,

    /// Compile to standalone executable
    #[arg(long)]
    pub compile: bool,

    /// Enable minification
    #[arg(long)]
    pub minify: bool,

    /// Target environment: browser, bun, node (default: bun)
    #[arg(long, default_value = "bun")]
    pub target: String,

    /// Output format: esm, cjs, iife (default: esm)
    #[arg(long, default_value = "esm")]
    pub format: String,

    /// External packages (won't be bundled)
    #[arg(long)]
    pub external: Vec<String>,

    /// Generate sourcemap: none, inline, external (default: none)
    #[arg(long, default_value = "none")]
    pub sourcemap: String,
}

impl BuildCommand {
    pub async fn run(self, config: &Config) -> Result<()> {
        // Validate arguments
        if self.compile && self.outdir.is_some() {
            return Err(anyhow::anyhow!(
                "Cannot use --compile with --outdir. Use --outfile instead."
            ));
        }

        if self.entrypoints.is_empty() {
            return Err(anyhow::anyhow!("At least one entrypoint is required"));
        }

        // For now, only support single entrypoint
        if self.entrypoints.len() > 1 {
            return Err(anyhow::anyhow!(
                "Multiple entrypoints not yet supported. Use a single entry file."
            ));
        }

        let entry = &self.entrypoints[0];
        if !entry.exists() {
            return Err(anyhow::anyhow!("Entry file not found: {}", entry.display()));
        }

        // Bundle the code
        let bundled = self.bundle(entry, config).await?;

        if self.compile {
            // Compile to standalone executable
            self.compile_executable(&bundled)?;
        } else {
            // Write bundled JavaScript
            self.write_bundle(&bundled, entry)?;
        }

        Ok(())
    }

    /// Bundle entry file and all its dependencies into a single string.
    async fn bundle(&self, entry: &Path, config: &Config) -> Result<String> {
        let source = std::fs::read_to_string(entry)
            .with_context(|| format!("Failed to read {}", entry.display()))?;

        let is_typescript = needs_transpilation(&entry.to_string_lossy());

        // Check for imports
        let imports = parse_imports(&source);

        if !imports.is_empty() {
            // Build module graph and bundle
            let loader_config = build_loader_config(entry, &config.modules);
            let loader = Arc::new(ModuleLoader::new(loader_config));
            let mut graph = ModuleGraph::new(loader.clone());

            // Load entry as file:// URL
            let entry_url = format!("file://{}", entry.canonicalize()?.display());
            graph.load(&entry_url).await?;

            // Bundle in topological order with proper CJS/ESM handling
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
                // Skip built-in modules
                if url.starts_with("node:")
                    || url.starts_with("builtin://node:")
                    || url.starts_with("otter:")
                {
                    continue;
                }

                // Skip external packages if configured
                if self.is_external(url) {
                    continue;
                }

                if let Some(node) = graph.get(url) {
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

            Ok(bundle_modules_mixed(modules))
        } else {
            // No imports - just transpile if needed
            if is_typescript {
                let result = transpile_typescript(&source)
                    .map_err(|e| anyhow::anyhow!("Transpilation error: {}", e))?;
                Ok(result.code)
            } else {
                Ok(source)
            }
        }
    }

    /// Check if a module URL should be treated as external.
    fn is_external(&self, url: &str) -> bool {
        for pattern in &self.external {
            if url.contains(pattern) {
                return true;
            }
        }
        false
    }

    /// Compile bundled code into a standalone executable.
    fn compile_executable(&self, code: &str) -> Result<()> {
        let output = self
            .outfile
            .clone()
            .unwrap_or_else(|| PathBuf::from("a.out"));

        // Get current otter executable as base
        let otter_exe = std::env::current_exe().context("Failed to get current executable path")?;

        // Embed code
        embedded::embed_code(&otter_exe, code.as_bytes(), &output)?;

        let size = std::fs::metadata(&output)?.len();
        println!(
            "  Compiled: {} ({:.2} MB)",
            output.display(),
            size as f64 / 1_048_576.0
        );

        Ok(())
    }

    /// Write bundled JavaScript to file or directory.
    fn write_bundle(&self, code: &str, entry: &Path) -> Result<()> {
        let output = if let Some(ref outfile) = self.outfile {
            outfile.clone()
        } else if let Some(ref outdir) = self.outdir {
            // Create output directory
            std::fs::create_dir_all(outdir)?;

            // Generate output filename from entry
            let stem = entry.file_stem().unwrap_or_default();
            outdir.join(format!("{}.js", stem.to_string_lossy()))
        } else {
            // Default: same name with .js extension
            let stem = entry.file_stem().unwrap_or_default();
            PathBuf::from(format!("{}.js", stem.to_string_lossy()))
        };

        std::fs::write(&output, code)?;

        let size = code.len();
        println!(
            "  Bundled: {} ({:.2} KB)",
            output.display(),
            size as f64 / 1024.0
        );

        Ok(())
    }
}

/// Build loader config from entry path and modules config.
fn build_loader_config(entry: &Path, modules: &crate::config::ModulesConfig) -> LoaderConfig {
    let base_dir = entry
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    LoaderConfig {
        base_dir,
        remote_allowlist: if modules.remote_allowlist.is_empty() {
            // Default remote allowlist
            vec![
                "https://esm.sh/*".to_string(),
                "https://cdn.skypack.dev/*".to_string(),
                "https://unpkg.com/*".to_string(),
            ]
        } else {
            modules.remote_allowlist.clone()
        },
        cache_dir: modules.cache_dir.clone().unwrap_or_else(|| {
            dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from(".cache"))
                .join("otter")
                .join("modules")
        }),
        import_map: modules.import_map.clone(),
        node_api_profile: NodeApiProfile::Full,
        ..Default::default()
    }
}
