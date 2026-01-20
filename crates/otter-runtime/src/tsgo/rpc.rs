//! RPC channel for tsgo communication.
//!
//! This module provides MessagePack-based RPC communication with the tsgo type checker
//! when running in API mode (`tsgo --api`).
//!
//! The protocol is based on Deno's typescript-go client:
//! - Messages are 3-element MessagePack arrays: [type, name, payload]
//! - Type is a u8 indicating message type
//! - Name and payload are binary arrays

use crate::error::{JscError, JscResult};
use regex::Regex;
use serde::de::DeserializeOwned;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::OnceLock;

/// Find TypeScript lib file in various locations.
///
/// Searches for TypeScript lib files in:
/// 1. node_modules/typescript/lib/ (relative to hint path if provided)
/// 2. node_modules/typescript/lib/ (relative to cwd)
/// 3. Global npm/pnpm/yarn installs
fn find_typescript_lib_file(lib_name: &str, search_root: Option<&Path>) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok();

    // Build search paths dynamically
    let mut search_paths: Vec<PathBuf> = Vec::new();

    // First priority: search relative to the provided search root (tsconfig directory)
    if let Some(root) = search_root {
        search_paths.push(root.join("node_modules/typescript/lib"));
        // Also check parent directories (for monorepos)
        if let Some(parent) = root.parent() {
            search_paths.push(parent.join("node_modules/typescript/lib"));
        }
    }

    // Second priority: search relative to cwd
    if let Some(ref cwd) = cwd {
        search_paths.push(cwd.join("node_modules/typescript/lib"));
        if let Some(parent) = cwd.parent() {
            search_paths.push(parent.join("node_modules/typescript/lib"));
            if let Some(grandparent) = parent.parent() {
                search_paths.push(grandparent.join("node_modules/typescript/lib"));
            }
        }
    }

    // Global locations
    search_paths.push(PathBuf::from("/usr/local/lib/node_modules/typescript/lib"));
    search_paths.push(PathBuf::from(
        "/opt/homebrew/lib/node_modules/typescript/lib",
    ));

    for base_path in &search_paths {
        if !base_path.exists() {
            continue;
        }
        let full_path = base_path.join(lib_name);
        if full_path.exists() {
            return Some(full_path);
        }
    }

    None
}

fn should_rewrite_node_prefix(_file_path: &Path) -> bool {
    // Disable node: prefix rewriting - we now handle node:* resolution
    // properly via the resolveModuleName callback
    false
}

fn rewrite_node_prefix(contents: &str) -> Option<String> {
    static REWRITE_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();

    let patterns = REWRITE_PATTERNS.get_or_init(|| {
        vec![
            Regex::new(r#"(\bfrom\s+)(['"])node:"#).expect("valid regex"),
            Regex::new(r#"(\bimport\s+)(['"])node:"#).expect("valid regex"),
            Regex::new(r#"(\bimport\s*\(\s*)(['"])node:"#).expect("valid regex"),
            Regex::new(r#"(\brequire\s*\(\s*)(['"])node:"#).expect("valid regex"),
        ]
    });

    let mut out = contents.to_string();
    let mut changed = false;

    for pattern in patterns {
        if pattern.is_match(&out) {
            out = pattern.replace_all(&out, "$1$2").to_string();
            changed = true;
        }
    }

    if changed { Some(out) } else { None }
}

fn find_node_types_index(start_dir: &Path) -> Option<PathBuf> {
    let mut current = start_dir;

    loop {
        let candidate = current.join("node_modules/@types/node/index.d.ts");
        if candidate.exists() {
            return Some(candidate);
        }

        current = current.parent()?;
    }
}

/// Find a @types package in node_modules.
/// Returns the path to the index.d.ts file if found.
fn find_types_package(package_name: &str, start_dir: &Path) -> Option<PathBuf> {
    let mut current = start_dir;

    loop {
        // Try @types/<package>
        let types_dir = current.join("node_modules/@types").join(package_name);
        if types_dir.exists() {
            // Check for index.d.ts
            let index_path = types_dir.join("index.d.ts");
            if index_path.exists() {
                return Some(index_path);
            }
            // Check package.json for types field
            let pkg_json_path = types_dir.join("package.json");
            if let Ok(contents) = std::fs::read_to_string(&pkg_json_path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let Some(types) = json.get("types").or_else(|| json.get("typings")) {
                        if let Some(types_file) = types.as_str() {
                            let types_path = types_dir.join(types_file);
                            if types_path.exists() {
                                return Some(types_path);
                            }
                        }
                    }
                }
            }
        }

        current = current.parent()?;
    }
}

/// Resolve a type reference directive (e.g., from `/// <reference types="..." />` or tsconfig types)
fn resolve_type_reference(type_ref: &str, containing_file: &Path) -> Option<serde_json::Value> {
    // If the path is a directory, use it directly; otherwise get its parent
    let start_dir = if containing_file.is_dir() {
        containing_file
    } else {
        containing_file.parent().unwrap_or_else(|| Path::new("."))
    };

    // Try to find in @types
    if let Some(types_path) = find_types_package(type_ref, start_dir) {
        let resolved_path = types_path.to_string_lossy().to_string();
        return Some(serde_json::json!({
            "primary": true,
            "resolvedFileName": resolved_path,
            "isExternalLibraryImport": true
        }));
    }

    // Also check for direct packages in node_modules with their own types
    // This handles packages like "bun-types" that have types directly in the package
    let mut current = start_dir;
    loop {
        let direct_pkg_dir = current.join("node_modules").join(type_ref);
        if direct_pkg_dir.exists() {
            // Check package.json for types field
            let pkg_json_path = direct_pkg_dir.join("package.json");
            if let Ok(contents) = std::fs::read_to_string(&pkg_json_path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let Some(types) = json.get("types").or_else(|| json.get("typings")) {
                        if let Some(types_file) = types.as_str() {
                            let types_path = direct_pkg_dir.join(types_file);
                            if types_path.exists() {
                                let resolved_path = types_path.to_string_lossy().to_string();
                                tracing::debug!(
                                    "Resolved type reference '{}' to direct package types at {:?}",
                                    type_ref,
                                    resolved_path
                                );
                                return Some(serde_json::json!({
                                    "primary": true,
                                    "resolvedFileName": resolved_path,
                                    "isExternalLibraryImport": true
                                }));
                            }
                        }
                    }
                }
            }
            // Check for index.d.ts
            let index_path = direct_pkg_dir.join("index.d.ts");
            if index_path.exists() {
                let resolved_path = index_path.to_string_lossy().to_string();
                tracing::debug!(
                    "Resolved type reference '{}' to direct package at {:?}",
                    type_ref,
                    resolved_path
                );
                return Some(serde_json::json!({
                    "primary": true,
                    "resolvedFileName": resolved_path,
                    "isExternalLibraryImport": true
                }));
            }
        }

        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }

    None
}

/// Resolve a module name to its type definitions.
/// This handles imports like `import { SQL } from "otter"` by finding @types/otter.
fn resolve_module_name(module_name: &str, containing_file: &Path) -> Option<serde_json::Value> {
    // If the path is a directory, use it directly; otherwise get its parent
    let start_dir = if containing_file.is_dir() {
        containing_file
    } else {
        containing_file.parent().unwrap_or_else(|| Path::new("."))
    };

    // Skip relative imports
    if module_name.starts_with('.') {
        return None;
    }

    // Handle node: prefixed imports by looking up @types/node
    let (package_name, subpath) = if let Some(rest) = module_name.strip_prefix("node:") {
        // For node:test -> look for @types/node/test.d.ts
        ("node", Some(rest))
    } else {
        // Extract the package name (e.g., "otter" from "otter" or "otter/sql" from "otter/sql")
        let parts: Vec<&str> = module_name.splitn(2, '/').collect();
        (parts[0], parts.get(1).copied())
    };

    // Try to find in @types
    let mut current = start_dir;
    while let Some(parent) = current.parent() {
        let types_dir = current.join("node_modules/@types").join(package_name);
        if types_dir.exists() {
            // For subpath imports like node:test, look for <subpath>.d.ts
            if let Some(sub) = subpath {
                let subpath_file = types_dir.join(format!("{}.d.ts", sub));
                if subpath_file.exists() {
                    let resolved_path = subpath_file.to_string_lossy().to_string();
                    tracing::debug!(
                        "Resolved module '{}' to types at {:?}",
                        module_name,
                        resolved_path
                    );
                    return Some(serde_json::json!({
                        "resolvedFileName": resolved_path,
                        "isExternalLibraryImport": true,
                        "extension": ".d.ts"
                    }));
                }
            }

            // Check for index.d.ts
            let index_path = types_dir.join("index.d.ts");
            if index_path.exists() {
                let resolved_path = index_path.to_string_lossy().to_string();
                tracing::debug!(
                    "Resolved module '{}' to types at {:?}",
                    module_name,
                    resolved_path
                );
                return Some(serde_json::json!({
                    "resolvedFileName": resolved_path,
                    "isExternalLibraryImport": true,
                    "extension": ".d.ts"
                }));
            }

            // Check package.json for types field
            let pkg_json_path = types_dir.join("package.json");
            if let Ok(contents) = std::fs::read_to_string(&pkg_json_path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let Some(types) = json.get("types").or_else(|| json.get("typings")) {
                        if let Some(types_file) = types.as_str() {
                            let types_path = types_dir.join(types_file);
                            if types_path.exists() {
                                let resolved_path = types_path.to_string_lossy().to_string();
                                tracing::debug!(
                                    "Resolved module '{}' to types at {:?}",
                                    module_name,
                                    resolved_path
                                );
                                return Some(serde_json::json!({
                                    "resolvedFileName": resolved_path,
                                    "isExternalLibraryImport": true,
                                    "extension": ".d.ts"
                                }));
                            }
                        }
                    }
                }
            }
        }

        // Also check @types/node for bare module names that might have had node: prefix stripped
        // This handles cases like import from "test" which was originally "node:test"
        if subpath.is_none() {
            let node_types_dir = current.join("node_modules/@types/node");
            if node_types_dir.exists() {
                let node_subpath_file = node_types_dir.join(format!("{}.d.ts", package_name));
                if node_subpath_file.exists() {
                    let resolved_path = node_subpath_file.to_string_lossy().to_string();
                    tracing::debug!(
                        "Resolved module '{}' to node types at {:?}",
                        module_name,
                        resolved_path
                    );
                    return Some(serde_json::json!({
                        "resolvedFileName": resolved_path,
                        "isExternalLibraryImport": true,
                        "extension": ".d.ts"
                    }));
                }
            }
        }

        // Also check for direct packages in node_modules with their own types
        // This handles packages like "bun-types" that have types directly in the package
        let direct_pkg_dir = current.join("node_modules").join(package_name);
        if direct_pkg_dir.exists() {
            // Check package.json for types field
            let pkg_json_path = direct_pkg_dir.join("package.json");
            if let Ok(contents) = std::fs::read_to_string(&pkg_json_path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let Some(types) = json.get("types").or_else(|| json.get("typings")) {
                        if let Some(types_file) = types.as_str() {
                            let types_path = direct_pkg_dir.join(types_file);
                            if types_path.exists() {
                                let resolved_path = types_path.to_string_lossy().to_string();
                                tracing::debug!(
                                    "Resolved module '{}' to direct package types at {:?}",
                                    module_name,
                                    resolved_path
                                );
                                return Some(serde_json::json!({
                                    "resolvedFileName": resolved_path,
                                    "isExternalLibraryImport": true,
                                    "extension": ".d.ts"
                                }));
                            }
                        }
                    }
                }
            }
            // Check for index.d.ts
            let index_path = direct_pkg_dir.join("index.d.ts");
            if index_path.exists() {
                let resolved_path = index_path.to_string_lossy().to_string();
                tracing::debug!(
                    "Resolved module '{}' to direct package at {:?}",
                    module_name,
                    resolved_path
                );
                return Some(serde_json::json!({
                    "resolvedFileName": resolved_path,
                    "isExternalLibraryImport": true,
                    "extension": ".d.ts"
                }));
            }
        }

        current = parent;
    }

    None
}

fn maybe_prepend_node_types(contents: String, file_path: &Path, had_node_prefix: bool) -> String {
    if !had_node_prefix {
        return contents;
    }

    let dir = match file_path.parent() {
        Some(dir) => dir,
        None => return contents,
    };

    let index_path = match find_node_types_index(dir) {
        Some(path) => path,
        None => return contents,
    };

    let reference_path = index_path.to_string_lossy().replace('\\', "/");
    let reference_line = format!("/// <reference path=\"{}\" />\n", reference_path);

    if contents.starts_with(&reference_line) {
        contents
    } else {
        format!("{}{}", reference_line, contents)
    }
}

/// Message types for the RPC protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// Request from client to tsgo
    Request = 1,
    /// Response to a callback from tsgo
    CallResponse = 2,
    /// Error response to a callback
    CallError = 3,
    /// Response from tsgo to client
    Response = 4,
    /// Error from tsgo
    Error = 5,
    /// Callback request from tsgo to client
    Call = 6,
}

impl TryFrom<u8> for MessageType {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, <MessageType as TryFrom<u8>>::Error> {
        match value {
            1 => Ok(MessageType::Request),
            2 => Ok(MessageType::CallResponse),
            3 => Ok(MessageType::CallError),
            4 => Ok(MessageType::Response),
            5 => Ok(MessageType::Error),
            6 => Ok(MessageType::Call),
            _ => Err(format!("Invalid message type: {}", value)),
        }
    }
}

/// RPC channel to a tsgo subprocess.
///
/// Communicates with tsgo using MessagePack-based protocol over stdin/stdout.
///
/// # Protocol
///
/// Messages are 3-element MessagePack arrays: `[type, name, payload]`
/// - `type`: u8 message type (Request, Response, Error, Call, etc.)
/// - `name`: binary array containing method name
/// - `payload`: binary array containing JSON-encoded parameters
pub struct TsgoChannel {
    child: Child,
    reader: BufReader<ChildStdout>,
    writer: BufWriter<ChildStdin>,
    /// Root directory for searching TypeScript lib files (usually tsconfig directory)
    lib_search_root: Option<PathBuf>,
}

impl TsgoChannel {
    /// Create a new RPC channel by spawning tsgo with `--api` flag.
    ///
    /// # Arguments
    ///
    /// * `tsgo_path` - Path to the tsgo binary
    ///
    /// # Errors
    ///
    /// Returns error if the process cannot be spawned or pipes cannot be obtained.
    pub fn new(tsgo_path: &Path) -> JscResult<Self> {
        let mut child = Command::new(tsgo_path)
            .arg("--api")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                JscError::internal(format!("Failed to spawn tsgo at {:?}: {}", tsgo_path, e))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| JscError::internal("Failed to capture tsgo stdin".to_string()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| JscError::internal("Failed to capture tsgo stdout".to_string()))?;

        Ok(Self {
            child,
            reader: BufReader::new(stdout),
            writer: BufWriter::new(stdin),
            lib_search_root: None,
        })
    }

    /// Set the root directory for searching TypeScript lib files.
    ///
    /// This should be set to the directory containing tsconfig.json
    /// so that lib files can be found relative to the project.
    pub fn set_lib_search_root(&mut self, root: PathBuf) {
        self.lib_search_root = Some(root);
    }

    /// Write a message to the tsgo process.
    fn write_message(&mut self, ty: MessageType, name: &[u8], payload: &[u8]) -> JscResult<()> {
        // Write 3-element array header
        rmp::encode::write_array_len(&mut self.writer, 3)
            .map_err(|e| JscError::internal(format!("Failed to write array header: {}", e)))?;

        // Write type as explicit uint8 (0xcc prefix), not fixint
        // tsgo expects the uint8 format specifically
        self.writer
            .write_all(&[0xcc, ty as u8])
            .map_err(|e| JscError::internal(format!("Failed to write message type: {}", e)))?;

        // Write name as binary
        rmp::encode::write_bin(&mut self.writer, name)
            .map_err(|e| JscError::internal(format!("Failed to write method name: {}", e)))?;

        // Write payload as binary
        rmp::encode::write_bin(&mut self.writer, payload)
            .map_err(|e| JscError::internal(format!("Failed to write payload: {}", e)))?;

        self.writer
            .flush()
            .map_err(|e| JscError::internal(format!("Failed to flush writer: {}", e)))?;

        Ok(())
    }

    /// Read a message from the tsgo process.
    fn read_message(&mut self) -> JscResult<(MessageType, Vec<u8>, Vec<u8>)> {
        tracing::trace!("Reading message from tsgo...");

        // Read array header
        let len = rmp::decode::read_array_len(&mut self.reader).map_err(|e| {
            JscError::internal(format!(
                "Failed to read array header: {} (maybe tsgo died or sent invalid data?)",
                e
            ))
        })?;
        tracing::trace!("Read array of length {}", len);

        if len != 3 {
            return Err(JscError::internal(format!(
                "Expected 3-element array, got {}",
                len
            )));
        }

        // Read type - tsgo sends uint8 format (0xcc prefix)
        let ty: u8 = rmp::decode::read_int(&mut self.reader)
            .map_err(|e| JscError::internal(format!("Failed to read message type: {}", e)))?;
        tracing::trace!("Read message type: {}", ty);

        let ty = MessageType::try_from(ty).map_err(JscError::internal)?;

        // Read name
        let name = self.read_bin()?;

        // Read payload
        let payload = self.read_bin()?;

        Ok((ty, name, payload))
    }

    /// Read a binary value from the reader.
    fn read_bin(&mut self) -> JscResult<Vec<u8>> {
        let len = rmp::decode::read_bin_len(&mut self.reader)
            .map_err(|e| JscError::internal(format!("Failed to read binary length: {}", e)))?;

        let mut buf = vec![0u8; len as usize];
        self.reader
            .read_exact(&mut buf)
            .map_err(|e| JscError::internal(format!("Failed to read binary data: {}", e)))?;

        Ok(buf)
    }

    /// Handle a callback from tsgo.
    ///
    /// Returns the JSON response to send back.
    fn handle_callback(&self, name: &str, payload: &str) -> JscResult<String> {
        match name {
            "readFile" => {
                // Parse the file path from the payload (it's a JSON string)
                let file_path: String =
                    serde_json::from_str(payload).unwrap_or_else(|_| payload.to_string());

                // Handle asset:/// URLs - these are TypeScript lib files
                // Map them to node_modules/typescript/lib/
                let actual_path = if file_path.starts_with("asset:///") {
                    let lib_name = file_path.strip_prefix("asset:///").unwrap();
                    // Search for TypeScript lib files in various locations
                    find_typescript_lib_file(lib_name, self.lib_search_root.as_deref())
                } else {
                    Some(PathBuf::from(&file_path))
                };

                // Try to read the file
                match actual_path.and_then(|p| std::fs::read_to_string(&p).ok().map(|c| (p, c))) {
                    Some((path, contents)) => {
                        // tsgo does not resolve `node:` specifiers reliably, so strip the prefix
                        // from import/export specifiers for user source files during type checks.
                        let contents = if should_rewrite_node_prefix(&path) {
                            let had_node_prefix = contents.contains("node:");
                            let contents = rewrite_node_prefix(&contents).unwrap_or(contents);
                            maybe_prepend_node_types(contents, &path, had_node_prefix)
                        } else {
                            contents
                        };

                        Ok(serde_json::to_string(&contents).unwrap_or_else(|_| "null".to_string()))
                    }
                    None => {
                        // File not found - return null
                        tracing::debug!("readFile: file not found: {}", file_path);
                        Ok("null".to_string())
                    }
                }
            }

            "getPackageJsonScopeIfApplicable" | "getPackageScopeForPath" => {
                // For simple type checking, we don't need package.json resolution
                // Return null to let tsgo use default behavior
                tracing::debug!("{}: returning null", name);
                Ok("null".to_string())
            }

            "resolveModuleName" => {
                // Parse the payload to get the module name and containing file
                // Payload format: { "moduleName": "otter", "containingFile": "/path/to/file.ts", ... }
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) {
                    let module_name = json
                        .get("moduleName")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let containing_file = json
                        .get("containingFile")
                        .and_then(|v| v.as_str())
                        .map(Path::new)
                        .or_else(|| self.lib_search_root.as_deref())
                        .unwrap_or_else(|| Path::new("."));

                    tracing::debug!(
                        "resolveModuleName: {} from {:?}",
                        module_name,
                        containing_file
                    );

                    if let Some(resolved) = resolve_module_name(module_name, containing_file) {
                        tracing::debug!("Resolved module '{}' to {:?}", module_name, resolved);
                        return Ok(
                            serde_json::to_string(&resolved).unwrap_or_else(|_| "null".to_string())
                        );
                    }
                }

                tracing::debug!("resolveModuleName: returning null for {}", payload);
                Ok("null".to_string())
            }

            "resolveTypeReferenceDirective" => {
                // Parse the payload to get the type reference name and containing file
                // Payload format: { "typeReferenceDirectiveName": "otter", "containingFile": "/path/to/file.ts", ... }
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) {
                    let type_ref = json
                        .get("typeReferenceDirectiveName")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    // Get containing file, falling back to lib_search_root for virtual files
                    let containing_file_str = json
                        .get("containingFile")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    // Use lib_search_root for virtual files like "__inferred type names__.ts"
                    let containing_file = if containing_file_str.contains("__inferred")
                        || !Path::new(containing_file_str).exists()
                    {
                        self.lib_search_root
                            .as_deref()
                            .unwrap_or_else(|| Path::new("."))
                    } else {
                        Path::new(containing_file_str)
                    };

                    tracing::debug!(
                        "resolveTypeReferenceDirective: {} from {:?}",
                        type_ref,
                        containing_file
                    );

                    if !type_ref.is_empty() {
                        if let Some(resolved) = resolve_type_reference(type_ref, containing_file) {
                            tracing::debug!(
                                "Resolved type reference '{}' to {:?}",
                                type_ref,
                                resolved
                            );
                            return Ok(serde_json::to_string(&resolved)
                                .unwrap_or_else(|_| "null".to_string()));
                        }
                    }
                }

                tracing::debug!(
                    "resolveTypeReferenceDirective: returning null for {}",
                    payload
                );
                Ok("null".to_string())
            }

            "getImpliedNodeFormatForFile" => {
                // Default to ESM for all files
                // 1 = ESM in tsgo's protocol
                tracing::debug!("getImpliedNodeFormatForFile: returning ESM");
                Ok("1".to_string())
            }

            "isNodeSourceFile" => {
                // Check if the file is from node_modules
                let file_path: String =
                    serde_json::from_str(payload).unwrap_or_else(|_| payload.to_string());
                let is_node = file_path.contains("node_modules");
                tracing::debug!("isNodeSourceFile: {} -> {}", file_path, is_node);
                Ok(is_node.to_string())
            }

            _ => {
                // Unknown callback - return null as a safe default
                tracing::warn!("Unknown callback '{}', returning null", name);
                Ok("null".to_string())
            }
        }
    }

    /// Send an RPC request and wait for response.
    ///
    /// # Arguments
    ///
    /// * `method` - The RPC method name (e.g., "configure", "getDiagnostics")
    /// * `payload` - JSON-encoded parameters string
    ///
    /// # Returns
    ///
    /// The response as a string (JSON-encoded).
    ///
    /// # Errors
    ///
    /// Returns error on I/O errors or RPC error response.
    pub fn request_sync(&mut self, method: &str, payload: String) -> JscResult<String> {
        tracing::debug!("tsgo RPC request: {} {}", method, payload);

        // Send request
        self.write_message(MessageType::Request, method.as_bytes(), payload.as_bytes())?;
        tracing::debug!("Request sent, waiting for response...");

        // Read response (handling callbacks)
        loop {
            let (ty, name, response_payload) = self.read_message()?;

            match ty {
                MessageType::Response => {
                    let name_str = String::from_utf8_lossy(&name);
                    if name_str != method {
                        return Err(JscError::internal(format!(
                            "Method name mismatch: expected '{}', got '{}'",
                            method, name_str
                        )));
                    }

                    let response = String::from_utf8(response_payload).map_err(|e| {
                        JscError::internal(format!("Failed to decode response: {}", e))
                    })?;

                    tracing::trace!("tsgo RPC response: {}", response);
                    return Ok(response);
                }
                MessageType::Error => {
                    let error_msg = String::from_utf8_lossy(&response_payload);
                    return Err(JscError::internal(format!("tsgo RPC error: {}", error_msg)));
                }
                MessageType::Call => {
                    // Handle callback from tsgo
                    let callback_name = String::from_utf8_lossy(&name);
                    let callback_payload = String::from_utf8_lossy(&response_payload);
                    tracing::debug!("tsgo callback: {} {:?}", callback_name, callback_payload);

                    let response = self.handle_callback(&callback_name, &callback_payload)?;
                    self.write_message(MessageType::CallResponse, &name, response.as_bytes())?;
                }
                _ => {
                    return Err(JscError::internal(format!(
                        "Unexpected message type: {:?}",
                        ty
                    )));
                }
            }
        }
    }

    /// Send an RPC request and deserialize the response.
    pub fn request<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> JscResult<T> {
        let payload = serde_json::to_string(&params)
            .map_err(|e| JscError::internal(format!("Failed to serialize params: {}", e)))?;

        let response = self.request_sync(method, payload)?;

        // Handle empty responses - try to deserialize as null
        if response.is_empty() {
            serde_json::from_str("null").map_err(|e| {
                JscError::internal(format!("Failed to deserialize empty response: {}", e))
            })
        } else {
            serde_json::from_str(&response)
                .map_err(|e| JscError::internal(format!("Failed to deserialize response: {}", e)))
        }
    }

    /// Check if the tsgo process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Get the process ID of the tsgo subprocess.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Shutdown the tsgo process gracefully.
    pub fn shutdown(mut self) -> JscResult<()> {
        tracing::debug!("Shutting down tsgo process");

        // Kill the process
        let _ = self.child.kill();
        let _ = self.child.wait();

        Ok(())
    }
}

impl Drop for TsgoChannel {
    fn drop(&mut self) {
        // Best-effort cleanup
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_type_conversion() {
        assert_eq!(MessageType::try_from(1u8).unwrap(), MessageType::Request);
        assert_eq!(MessageType::try_from(4u8).unwrap(), MessageType::Response);
        assert_eq!(MessageType::try_from(5u8).unwrap(), MessageType::Error);
        assert_eq!(MessageType::try_from(6u8).unwrap(), MessageType::Call);
        assert!(MessageType::try_from(99u8).is_err());
    }
}
