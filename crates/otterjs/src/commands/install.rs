//! Install command - install dependencies from package.json.

use anyhow::Result;
use clap::Args;
use otter_pm::Installer;

#[derive(Args)]
pub struct InstallCommand {
    /// Install specific packages (if not provided, installs from package.json)
    #[arg()]
    pub packages: Vec<String>,

    /// Save as production dependency
    #[arg(long, short = 'S')]
    pub save: bool,

    /// Save as dev dependency
    #[arg(long, short = 'D')]
    pub save_dev: bool,

    /// Install production dependencies only
    #[arg(long)]
    pub production: bool,

    /// Don't save to package.json
    #[arg(long)]
    pub no_save: bool,
}

impl InstallCommand {
    pub async fn run(&self) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let package_json = cwd.join("package.json");

        if !package_json.exists() {
            anyhow::bail!(
                "No package.json found in current directory.\n\
                 Run 'otter init' to create a new project."
            );
        }

        // If specific packages provided, install them
        if !self.packages.is_empty() {
            return self.install_packages(&cwd).await;
        }

        // Otherwise install from package.json
        println!("Installing dependencies...");

        let mut installer = Installer::new(&cwd);
        installer
            .install(&package_json)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        println!("Dependencies installed successfully.");
        Ok(())
    }

    async fn install_packages(&self, cwd: &std::path::Path) -> Result<()> {
        let package_json_path = cwd.join("package.json");

        // Read existing package.json
        let content = std::fs::read_to_string(&package_json_path)?;
        let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

        let deps_key = if self.save_dev {
            "devDependencies"
        } else {
            "dependencies"
        };

        // Ensure dependencies object exists
        if pkg.get(deps_key).is_none() {
            pkg[deps_key] = serde_json::json!({});
        }

        for package in &self.packages {
            // Parse package@version format
            let (name, version) = if let Some(idx) = package.rfind('@') {
                if idx > 0 {
                    (&package[..idx], &package[idx + 1..])
                } else {
                    (package.as_str(), "latest")
                }
            } else {
                (package.as_str(), "latest")
            };

            println!("Installing {}@{}...", name, version);

            // Add to package.json
            if !self.no_save {
                let version_spec = if version == "latest" {
                    "^0.0.0".to_string() // Will be resolved during install
                } else {
                    format!("^{}", version)
                };
                pkg[deps_key][name] = serde_json::Value::String(version_spec);
            }
        }

        // Write updated package.json
        if !self.no_save {
            std::fs::write(&package_json_path, serde_json::to_string_pretty(&pkg)?)?;
        }

        // Run install
        let mut installer = Installer::new(cwd);
        installer
            .install(&package_json_path)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        println!("Packages installed successfully.");
        Ok(())
    }
}
