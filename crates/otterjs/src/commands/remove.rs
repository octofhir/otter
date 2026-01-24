//! Remove command - remove a package from package.json.

use anyhow::Result;

/// Run the remove command
pub async fn run(package: &str) -> Result<()> {
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

    let mut found = false;

    // Check in dependencies
    if let Some(deps) = pkg.get_mut("dependencies")
        && let Some(obj) = deps.as_object_mut()
        && obj.remove(package).is_some()
    {
        found = true;
    }

    // Check in devDependencies
    if let Some(deps) = pkg.get_mut("devDependencies")
        && let Some(obj) = deps.as_object_mut()
        && obj.remove(package).is_some()
    {
        found = true;
    }

    // Check in peerDependencies
    if let Some(deps) = pkg.get_mut("peerDependencies")
        && let Some(obj) = deps.as_object_mut()
        && obj.remove(package).is_some()
    {
        found = true;
    }

    // Check in optionalDependencies
    if let Some(deps) = pkg.get_mut("optionalDependencies")
        && let Some(obj) = deps.as_object_mut()
        && obj.remove(package).is_some()
    {
        found = true;
    }

    if !found {
        println!("Package '{}' not found in dependencies.", package);
        return Ok(());
    }

    // Write updated package.json
    std::fs::write(&package_json_path, serde_json::to_string_pretty(&pkg)?)?;

    // Remove from node_modules if it exists
    let node_modules = cwd.join("node_modules");
    if node_modules.exists() {
        let pkg_dir = node_modules.join(package);
        if pkg_dir.exists() {
            std::fs::remove_dir_all(&pkg_dir)?;
        }
    }

    println!("Removed {}", package);
    Ok(())
}
