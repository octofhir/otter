//! Bundled TypeScript type definitions management
//!
//! This module handles copying Otter's bundled type definitions to node_modules/@types
//! so that editors can provide IntelliSense and go-to-definition support.
//!
//! Note: @types/node is NOT bundled. Instead, @types/otter depends on @types/node from npm.
//! Web API types (fetch, URL, etc.) come from @types/node.

use std::fs;
use std::path::Path;

fn write_if_changed(path: &Path, contents: &str) -> Result<(), TypesError> {
    if let Ok(existing) = fs::read(path) {
        if existing == contents.as_bytes() {
            return Ok(());
        }
    }

    fs::write(path, contents).map_err(|e| TypesError::Io(e.to_string()))?;
    Ok(())
}

/// Otter global API types (CommonJS support)
const OTTER_GLOBALS_TYPES: &str = include_str!("types/otter/globals.d.ts");

/// Otter index.d.ts (entry point)
const OTTER_INDEX_TYPES: &str = include_str!("types/otter/index.d.ts");

/// Otter SQL and KV types
const OTTER_SQL_TYPES: &str = include_str!("types/otter/sql.d.ts");

/// Otter serve types
const OTTER_SERVE_TYPES: &str = include_str!("types/otter/serve.d.ts");

/// Install bundled type definitions to node_modules
pub fn install_bundled_types(node_modules: &Path) -> Result<(), TypesError> {
    install_otter_types(node_modules)
}

/// Install otter-types (Otter-specific APIs)
fn install_otter_types(node_modules: &Path) -> Result<(), TypesError> {
    let types_dir = node_modules.join("otter-types");
    fs::create_dir_all(&types_dir).map_err(|e| TypesError::Io(e.to_string()))?;

    // Write type definition files
    write_if_changed(&types_dir.join("index.d.ts"), OTTER_INDEX_TYPES)?;
    write_if_changed(&types_dir.join("globals.d.ts"), OTTER_GLOBALS_TYPES)?;
    write_if_changed(&types_dir.join("sql.d.ts"), OTTER_SQL_TYPES)?;
    write_if_changed(&types_dir.join("serve.d.ts"), OTTER_SERVE_TYPES)?;

    // Write package.json with @types/node dependency
    let package_json = r#"{
  "name": "otter-types",
  "version": "0.1.1",
  "description": "TypeScript definitions for Otter runtime",
  "types": "index.d.ts",
  "license": "MIT",
  "repository": {
    "type": "git",
    "url": "https://github.com/octofhir/otter",
    "directory": "packages/otter-types"
  },
  "dependencies": {
    "@types/node": "*"
  }
}"#;
    write_if_changed(&types_dir.join("package.json"), package_json)?;

    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum TypesError {
    #[error("IO error: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundled_types_not_empty() {
        assert!(!OTTER_INDEX_TYPES.is_empty());
        assert!(!OTTER_GLOBALS_TYPES.is_empty());
        assert!(!OTTER_SQL_TYPES.is_empty());
        assert!(!OTTER_SERVE_TYPES.is_empty());
    }

    #[test]
    fn test_install_bundled_types() {
        let temp_dir =
            std::env::temp_dir().join(format!("otter-types-test-{}", std::process::id()));
        let node_modules = temp_dir.join("node_modules");

        // Clean up first
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&node_modules).unwrap();

        // Install types
        install_bundled_types(&node_modules).unwrap();

        // Verify otter-types
        assert!(node_modules.join("otter-types/index.d.ts").exists());
        assert!(node_modules.join("otter-types/globals.d.ts").exists());
        assert!(node_modules.join("otter-types/sql.d.ts").exists());
        assert!(node_modules.join("otter-types/serve.d.ts").exists());
        assert!(node_modules.join("otter-types/package.json").exists());

        // Verify package.json has @types/node dependency
        let pkg_json = fs::read_to_string(node_modules.join("otter-types/package.json")).unwrap();
        assert!(pkg_json.contains("\"@types/node\""));

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
