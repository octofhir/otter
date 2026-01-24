//! Install command - install dependencies from package.json.

use anyhow::Result;
use otter_pm::Installer;

/// Run the install command
pub async fn run(packages: &[String], save_dev: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let package_json = cwd.join("package.json");

    if !package_json.exists() {
        anyhow::bail!(
            "No package.json found in current directory.\n\
             Run 'otter init' to create a new project."
        );
    }

    // If specific packages provided, install them
    if !packages.is_empty() {
        return install_packages(&cwd, packages, save_dev).await;
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

async fn install_packages(
    cwd: &std::path::Path,
    packages: &[String],
    save_dev: bool,
) -> Result<()> {
    let package_json_path = cwd.join("package.json");

    // Read existing package.json
    let content = std::fs::read_to_string(&package_json_path)?;
    let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

    let deps_key = if save_dev {
        "devDependencies"
    } else {
        "dependencies"
    };

    // Ensure dependencies object exists
    if pkg.get(deps_key).is_none() {
        pkg[deps_key] = serde_json::json!({});
    }

    for package in packages {
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
        let version_spec = if version == "latest" {
            "^0.0.0".to_string()
        } else {
            format!("^{}", version)
        };
        pkg[deps_key][name] = serde_json::Value::String(version_spec);
    }

    // Write updated package.json
    std::fs::write(&package_json_path, serde_json::to_string_pretty(&pkg)?)?;

    // Run install
    let mut installer = Installer::new(cwd);
    installer
        .install(&package_json_path)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    println!("Packages installed successfully.");
    Ok(())
}
