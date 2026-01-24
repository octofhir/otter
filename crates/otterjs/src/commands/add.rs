//! Add command - add a package to package.json.

use anyhow::Result;
use otter_pm::Installer;

/// Run the add command
pub async fn run(package: &str, dev: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let package_json_path = cwd.join("package.json");

    if !package_json_path.exists() {
        anyhow::bail!(
            "No package.json found in current directory.\n\
             Run 'otter init' to create a new project."
        );
    }

    // Parse package@version format
    let (name, version) = parse_package_spec(package);

    // Read and update package.json
    let content = std::fs::read_to_string(&package_json_path)?;
    let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

    let deps_key = if dev {
        "devDependencies"
    } else {
        "dependencies"
    };

    // Ensure dependencies object exists
    if pkg.get(deps_key).is_none() {
        pkg[deps_key] = serde_json::json!({});
    }

    println!("Adding {}@{}...", name, version);

    // Add package
    let version_spec = if version == "latest" {
        "^0.0.0".to_string()
    } else {
        format!("^{}", version)
    };
    pkg[deps_key][name] = serde_json::Value::String(version_spec);

    // Write updated package.json
    std::fs::write(&package_json_path, serde_json::to_string_pretty(&pkg)?)?;

    // Run install
    let mut installer = Installer::new(&cwd);
    installer
        .install(&package_json_path)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    println!("Added {} successfully.", name);
    Ok(())
}

fn parse_package_spec(package: &str) -> (&str, &str) {
    // Handle @scoped/package@version
    if let Some(rest) = package.strip_prefix('@') {
        if let Some(idx) = rest.find('@') {
            let split_idx = idx + 1;
            return (&package[..split_idx], &package[split_idx + 1..]);
        }
    } else if let Some(idx) = package.rfind('@') {
        return (&package[..idx], &package[idx + 1..]);
    }

    (package, "latest")
}
