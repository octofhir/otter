//! Add command - add a dependency to the project.

use anyhow::Result;
use clap::Args;
use otter_pm::Installer;

#[derive(Args)]
pub struct AddCommand {
    /// Packages to add (e.g., lodash, react@18.0.0)
    #[arg(required = true)]
    pub packages: Vec<String>,

    /// Add as dev dependency
    #[arg(long, short = 'D')]
    pub dev: bool,

    /// Exact version (don't use ^ prefix)
    #[arg(long, short = 'E')]
    pub exact: bool,
}

impl AddCommand {
    pub async fn run(&self) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let package_json_path = cwd.join("package.json");

        if !package_json_path.exists() {
            anyhow::bail!(
                "No package.json found in current directory.\n\
                 Run 'otter init' to create a new project."
            );
        }

        // Read existing package.json
        let content = std::fs::read_to_string(&package_json_path)?;
        let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

        let deps_key = if self.dev {
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
            let (name, version) = self.parse_package_spec(package);

            println!("Adding {}@{}...", name, version);

            // Determine version specifier
            let version_spec = if self.exact || version != "latest" {
                version.to_string()
            } else {
                format!("^{}", version)
            };

            pkg[deps_key][name] = serde_json::Value::String(version_spec);
        }

        // Write updated package.json
        std::fs::write(&package_json_path, serde_json::to_string_pretty(&pkg)?)?;

        // Run install
        let mut installer = Installer::new(&cwd);
        installer
            .install(&package_json_path)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let count = self.packages.len();
        if count == 1 {
            println!("Added 1 package.");
        } else {
            println!("Added {} packages.", count);
        }

        Ok(())
    }

    fn parse_package_spec<'a>(&self, package: &'a str) -> (&'a str, &'a str) {
        // Handle @scoped/package@version
        if let Some(rest) = package.strip_prefix('@') {
            // Find the second @ (if any) after the scope prefix
            if let Some(idx) = rest.find('@') {
                let split_idx = idx + 1; // Account for the '@' we stripped
                return (&package[..split_idx], &package[split_idx + 1..]);
            }
        } else if let Some(idx) = package.rfind('@') {
            return (&package[..idx], &package[idx + 1..]);
        }

        (package, "latest")
    }
}
