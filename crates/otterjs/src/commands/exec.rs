//! Exec command - execute a package binary (npx-like functionality).
//!
//! Usage:
//!   otter x cowsay "Hello"        # Run cowsay, download if needed
//!   otter x typescript --help     # Use local node_modules/.bin first
//!   otter x esbuild@0.19 app.ts   # Specific version
//!   otter x -y serve ./dist       # Skip confirmation
//!   otter x --no-install tsc      # Local only, fail if not found

use anyhow::Result;
use clap::Args;
use otter_pm::{BinResolver, Installer, NpmRegistry};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Well-known packages that don't need confirmation
const TRUSTED_PACKAGES: &[&str] = &[
    "typescript",
    "tsc",
    "ts-node",
    "tsx",
    "eslint",
    "prettier",
    "jest",
    "vitest",
    "webpack",
    "vite",
    "esbuild",
    "rollup",
    "serve",
    "http-server",
    "create-react-app",
    "create-next-app",
    "create-vite",
    "npx",
    "npm",
    "yarn",
    "pnpm",
];

#[derive(Args)]
pub struct ExecCommand {
    /// Package[@version] to execute
    pub package: String,

    /// Arguments to pass to the binary
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// Skip downloading, only use locally installed packages
    #[arg(long)]
    pub no_install: bool,

    /// Specific command to run (if package has multiple bins)
    #[arg(long, short)]
    pub command: Option<String>,

    /// Skip confirmation prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
}

impl ExecCommand {
    pub async fn run(&self) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let resolver = BinResolver::new(&cwd);

        // Parse package name and version
        let (package_name, version_spec) = parse_package_spec(&self.package);
        let cmd_name = self.command.as_deref().unwrap_or(&package_name);

        // 1. Try local node_modules/.bin first
        if let Some(bin) = resolver.find_local(cmd_name) {
            return self.execute_binary(&bin.path);
        }

        // 2. If --no-install, fail here
        if self.no_install {
            return Err(anyhow::anyhow!(
                "Binary '{}' not found in local node_modules/.bin\n\n\
                 To download and run, remove --no-install flag:\n  \
                 otter x {}",
                cmd_name,
                self.package
            ));
        }

        // 3. Check if in global cache
        let version = if let Some(v) = version_spec {
            v.to_string()
        } else {
            // Fetch latest version
            self.fetch_latest_version(&package_name).await?
        };

        if let Some(bin) = resolver.find_cached(&package_name, &version, cmd_name) {
            return self.execute_binary(&bin.path);
        }

        // 4. Need to download - confirm with user
        if !self.yes && !is_trusted(&package_name) {
            if !confirm_install(&package_name, &version)? {
                return Err(anyhow::anyhow!("Installation cancelled"));
            }
        }

        // 5. Download and install to cache
        let cache_path = self.install_to_cache(&package_name, &version).await?;

        // 6. Find and execute the binary
        if let Some(bin) = resolver.find_cached(&package_name, &version, cmd_name) {
            return self.execute_binary(&bin.path);
        }

        // Try to find the binary in the installed package
        let bin_path = self.find_bin_in_package(&cache_path, &package_name, cmd_name)?;
        self.execute_binary(&bin_path)
    }

    /// Execute a binary with arguments
    fn execute_binary(&self, bin_path: &Path) -> Result<()> {
        let status = Command::new(bin_path)
            .args(&self.args)
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

    /// Fetch the latest version of a package from npm
    async fn fetch_latest_version(&self, package: &str) -> Result<String> {
        let registry = NpmRegistry::new();
        let metadata = registry.get_package(package).await?;

        metadata
            .dist_tags
            .get("latest")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No latest version found for '{}'", package))
    }

    /// Install package to global cache
    async fn install_to_cache(&self, package: &str, version: &str) -> Result<PathBuf> {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("otter/exec-cache");

        let safe_name = package.replace('/', "-").replace('@', "");
        let pkg_cache = cache_dir.join(&safe_name).join(version);

        // Create cache directory
        std::fs::create_dir_all(&pkg_cache)?;

        // Create a minimal package.json for the install
        let pkg_json_path = pkg_cache.join("package.json");
        let pkg_json = format!(
            r#"{{"name": "otter-exec-temp", "dependencies": {{"{}": "{}"}}}}"#,
            package, version
        );
        std::fs::write(&pkg_json_path, pkg_json)?;

        // Install using the existing installer
        let mut installer = Installer::new(&pkg_cache);

        // Run install
        installer.install(&pkg_json_path).await?;

        // Mark as installed
        std::fs::write(pkg_cache.join(".installed"), "")?;

        Ok(pkg_cache)
    }

    /// Find the binary in an installed package
    fn find_bin_in_package(&self, cache_path: &Path, package: &str, cmd: &str) -> Result<PathBuf> {
        // Check node_modules/.bin
        let bin_dir = cache_path.join("node_modules/.bin");
        let bin_path = bin_dir.join(cmd);
        if bin_path.exists() {
            return Ok(bin_path);
        }

        // Check package's bin field
        let pkg_dir = cache_path.join("node_modules").join(package);
        let pkg_json_path = pkg_dir.join("package.json");

        if pkg_json_path.exists() {
            let content = std::fs::read_to_string(&pkg_json_path)?;
            let pkg: otter_pm::PackageJson = serde_json::from_str(&content)?;

            if let Some(ref bin) = pkg.bin {
                let bins = bin.to_map(package);
                if let Some(bin_rel) = bins.get(cmd) {
                    return Ok(pkg_dir.join(bin_rel));
                }
            }
        }

        Err(anyhow::anyhow!(
            "Binary '{}' not found in package '{}'",
            cmd,
            package
        ))
    }
}

/// Parse package[@version] spec
fn parse_package_spec(spec: &str) -> (String, Option<&str>) {
    // Handle scoped packages: @scope/pkg@version
    if spec.starts_with('@') {
        if let Some(at_idx) = spec[1..].find('@') {
            let split_idx = at_idx + 1;
            let name = &spec[..split_idx];
            let version = &spec[split_idx + 1..];
            return (name.to_string(), Some(version));
        }
        return (spec.to_string(), None);
    }

    // Regular package: pkg@version
    if let Some((name, version)) = spec.split_once('@') {
        (name.to_string(), Some(version))
    } else {
        (spec.to_string(), None)
    }
}

/// Check if package is trusted (skip confirmation)
fn is_trusted(package: &str) -> bool {
    // Remove scope if present
    let name = package.split('/').last().unwrap_or(package);
    TRUSTED_PACKAGES.contains(&name) || package.starts_with("@types/")
}

/// Prompt user for confirmation
fn confirm_install(package: &str, version: &str) -> io::Result<bool> {
    // Non-interactive mode (piped input) - deny by default
    if !atty::is(atty::Stream::Stdin) {
        eprintln!(
            "\x1b[33motter x\x1b[0m: Package '{}@{}' requires download.\n\
             Use -y to skip confirmation in non-interactive mode.",
            package, version
        );
        return Ok(false);
    }

    print!(
        "\x1b[33motter x\x1b[0m: Package '{}@{}' is not installed.\n\
         Install and run? (y/N) ",
        package, version
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(input.trim().eq_ignore_ascii_case("y"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_package_spec() {
        assert_eq!(parse_package_spec("cowsay"), ("cowsay".into(), None));
        assert_eq!(
            parse_package_spec("cowsay@1.5.0"),
            ("cowsay".into(), Some("1.5.0"))
        );
        assert_eq!(
            parse_package_spec("@types/node"),
            ("@types/node".into(), None)
        );
        assert_eq!(
            parse_package_spec("@types/node@18.0.0"),
            ("@types/node".into(), Some("18.0.0"))
        );
    }

    #[test]
    fn test_is_trusted() {
        assert!(is_trusted("typescript"));
        assert!(is_trusted("eslint"));
        assert!(is_trusted("@types/node"));
        assert!(!is_trusted("random-package"));
    }
}
