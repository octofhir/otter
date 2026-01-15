//! Bundled TypeScript type definitions management
//!
//! This module handles copying Otter's bundled type definitions to node_modules/@types
//! so that editors can provide IntelliSense and go-to-definition support.

use std::fs;
use std::path::Path;

/// Otter global API types (console, timers, fetch, etc.)
const OTTER_TYPES: &str = include_str!("../../otter-runtime/types/otter.d.ts");

/// Node.js Buffer types
const NODE_BUFFER_TYPES: &str = include_str!("../../otter-runtime/types/node/buffer.d.ts");

/// Node.js fs types
const NODE_FS_TYPES: &str = include_str!("../../otter-runtime/types/node/fs.d.ts");

/// Node.js path types
const NODE_PATH_TYPES: &str = include_str!("../../otter-runtime/types/node/path.d.ts");

/// Node.js test types
const NODE_TEST_TYPES: &str = include_str!("../../otter-runtime/types/node/test.d.ts");

/// Install bundled type definitions to node_modules
pub fn install_bundled_types(node_modules: &Path) -> Result<(), TypesError> {
    // Install @types/otter for global APIs
    install_otter_types(node_modules)?;

    // Install @types/node for Node.js compatibility APIs
    install_node_types(node_modules)?;

    Ok(())
}

/// Install @types/otter (global Otter APIs)
fn install_otter_types(node_modules: &Path) -> Result<(), TypesError> {
    let types_dir = node_modules.join("@types").join("otter");
    fs::create_dir_all(&types_dir).map_err(|e| TypesError::Io(e.to_string()))?;

    // Write index.d.ts
    fs::write(types_dir.join("index.d.ts"), OTTER_TYPES)
        .map_err(|e| TypesError::Io(e.to_string()))?;

    // Write package.json
    let package_json = r#"{
  "name": "@types/otter",
  "version": "0.1.0",
  "description": "TypeScript definitions for Otter runtime",
  "types": "index.d.ts",
  "license": "MIT"
}"#;
    fs::write(types_dir.join("package.json"), package_json)
        .map_err(|e| TypesError::Io(e.to_string()))?;

    Ok(())
}

/// Install @types/node (Node.js compatibility APIs)
fn install_node_types(node_modules: &Path) -> Result<(), TypesError> {
    let types_dir = node_modules.join("@types").join("node");
    fs::create_dir_all(&types_dir).map_err(|e| TypesError::Io(e.to_string()))?;

    // Write individual module types
    fs::write(types_dir.join("buffer.d.ts"), NODE_BUFFER_TYPES)
        .map_err(|e| TypesError::Io(e.to_string()))?;

    fs::write(types_dir.join("fs.d.ts"), NODE_FS_TYPES)
        .map_err(|e| TypesError::Io(e.to_string()))?;

    fs::write(types_dir.join("path.d.ts"), NODE_PATH_TYPES)
        .map_err(|e| TypesError::Io(e.to_string()))?;

    fs::write(types_dir.join("test.d.ts"), NODE_TEST_TYPES)
        .map_err(|e| TypesError::Io(e.to_string()))?;

    // Write index.d.ts that re-exports all modules
    let index_dts = r#"/// <reference path="buffer.d.ts" />
/// <reference path="fs.d.ts" />
/// <reference path="path.d.ts" />
/// <reference path="test.d.ts" />

// Re-export modules for direct imports
export * from "node:buffer";
export * from "node:fs";
export * from "node:path";
export * from "node:test";
"#;
    fs::write(types_dir.join("index.d.ts"), index_dts)
        .map_err(|e| TypesError::Io(e.to_string()))?;

    // Write package.json
    let package_json = r#"{
  "name": "@types/node",
  "version": "0.1.0",
  "description": "TypeScript definitions for Otter's Node.js compatibility layer",
  "types": "index.d.ts",
  "license": "MIT"
}"#;
    fs::write(types_dir.join("package.json"), package_json)
        .map_err(|e| TypesError::Io(e.to_string()))?;

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
        assert!(!OTTER_TYPES.is_empty());
        assert!(!NODE_BUFFER_TYPES.is_empty());
        assert!(!NODE_FS_TYPES.is_empty());
        assert!(!NODE_PATH_TYPES.is_empty());
    }

    #[test]
    fn test_install_bundled_types() {
        let temp_dir = std::env::temp_dir().join("otter-types-test");
        let node_modules = temp_dir.join("node_modules");

        // Clean up first
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&node_modules).unwrap();

        // Install types
        install_bundled_types(&node_modules).unwrap();

        // Verify @types/otter
        assert!(node_modules.join("@types/otter/index.d.ts").exists());
        assert!(node_modules.join("@types/otter/package.json").exists());

        // Verify @types/node
        assert!(node_modules.join("@types/node/index.d.ts").exists());
        assert!(node_modules.join("@types/node/fs.d.ts").exists());
        assert!(node_modules.join("@types/node/buffer.d.ts").exists());
        assert!(node_modules.join("@types/node/path.d.ts").exists());

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
