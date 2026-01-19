//! Embedded code support for standalone executables.
//!
//! When `otter build --compile` creates a standalone executable, it appends
//! the bundled JavaScript code to the otter binary with a trailer that marks
//! the embedded content. At startup, we check for this trailer and if found,
//! run the embedded code instead of the normal CLI.
//!
//! Binary layout:
//! ```text
//! ┌─────────────────────────┐
//! │   Otter binary          │  ← Original executable
//! ├─────────────────────────┤
//! │   Bundled JS Code       │  ← Appended by --compile
//! ├─────────────────────────┤
//! │   Trailer (16 bytes)    │
//! │   - magic: "OTTER\0\0\0"│
//! │   - offset: u64 (LE)    │
//! └─────────────────────────┘
//! ```

use anyhow::Result;
use otter_engine::{Capabilities, EnvStoreBuilder};
use otter_node::{ext, ProcessInfo};
use otter_runtime::{JscConfig, JscRuntime, set_tokio_handle};
use std::sync::Arc;
use std::time::Duration;

/// Magic bytes marking an embedded Otter executable
const MAGIC: &[u8; 8] = b"OTTER\0\0\0";

/// Trailer size in bytes (magic + offset)
const TRAILER_SIZE: usize = 16;

/// Load embedded code from the current executable.
///
/// Returns `Ok(Some(code))` if embedded code is found,
/// `Ok(None)` if this is a regular otter binary,
/// `Err` on I/O or parse errors.
pub fn load_embedded_code() -> Result<Option<String>> {
    let exe = std::env::current_exe()?;
    let data = std::fs::read(&exe)?;

    if data.len() < TRAILER_SIZE {
        return Ok(None);
    }

    // Check trailer at end of file
    let trailer = &data[data.len() - TRAILER_SIZE..];

    // Verify magic bytes
    if &trailer[0..8] != MAGIC {
        return Ok(None);
    }

    // Read offset (little-endian u64)
    let offset_bytes: [u8; 8] = trailer[8..16].try_into()?;
    let offset = u64::from_le_bytes(offset_bytes) as usize;

    // Validate offset
    if offset >= data.len() - TRAILER_SIZE {
        return Err(anyhow::anyhow!(
            "Invalid embedded code offset: {} (file size: {})",
            offset,
            data.len()
        ));
    }

    // Extract code between offset and trailer
    let code_bytes = &data[offset..data.len() - TRAILER_SIZE];
    let code = String::from_utf8(code_bytes.to_vec())
        .map_err(|e| anyhow::anyhow!("Embedded code is not valid UTF-8: {}", e))?;

    Ok(Some(code))
}

/// Embed code into an otter binary, creating a standalone executable.
///
/// This copies the source binary, appends the code, and writes a trailer
/// with the magic bytes and offset.
pub fn embed_code(otter_exe: &std::path::Path, code: &[u8], output: &std::path::Path) -> Result<()> {
    let mut binary = std::fs::read(otter_exe)?;

    // Strip any existing embedded code (allows recompiling)
    binary = strip_embedded(&binary);

    let code_offset = binary.len() as u64;

    // Append code
    binary.extend_from_slice(code);

    // Append trailer: magic + offset
    binary.extend_from_slice(MAGIC);
    binary.extend_from_slice(&code_offset.to_le_bytes());

    // Write output
    std::fs::write(output, &binary)?;

    // Make executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(())
}

/// Strip embedded code from a binary if present.
///
/// Returns the original binary without any appended code.
fn strip_embedded(binary: &[u8]) -> Vec<u8> {
    if binary.len() < TRAILER_SIZE {
        return binary.to_vec();
    }

    let trailer = &binary[binary.len() - TRAILER_SIZE..];

    // Check for magic
    if &trailer[0..8] == MAGIC {
        let offset_bytes: [u8; 8] = trailer[8..16].try_into().unwrap();
        let offset = u64::from_le_bytes(offset_bytes) as usize;

        // Validate and truncate
        if offset < binary.len() - TRAILER_SIZE {
            return binary[..offset].to_vec();
        }
    }

    binary.to_vec()
}

/// Run embedded code with full runtime support.
///
/// This initializes all extensions and runs the event loop,
/// providing the same capabilities as `otter run`.
pub async fn run_embedded(code: String) -> Result<()> {
    // Set tokio handle for async operations
    set_tokio_handle(tokio::runtime::Handle::current());

    // Create runtime
    let runtime = JscRuntime::new(JscConfig::default())?;

    // Register all extensions (same as run.rs)
    // Web API extensions
    runtime.register_extension(ext::url())?;

    // Node.js compatibility extensions
    runtime.register_extension(ext::path())?;
    runtime.register_extension(ext::buffer())?;

    // Allow all capabilities for standalone executables
    // (permissions were checked at compile time)
    let caps = Capabilities::all();
    runtime.register_extension(ext::fs(caps.clone()))?;

    runtime.register_extension(ext::test())?;
    runtime.register_extension(ext::events())?;
    runtime.register_extension(ext::crypto())?;
    runtime.register_extension(ext::util())?;
    runtime.register_extension(ext::process())?;
    runtime.register_extension(ext::os())?;
    runtime.register_extension(ext::child_process())?;
    runtime.register_extension(ext::net())?;

    // HTTP server extension
    let (http_event_tx, _http_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (http_server_ext, _active_count) = ext::http_server(http_event_tx);
    runtime.register_extension(http_server_ext)?;
    runtime.register_extension(ext::http())?;

    // SQL and KV extensions
    runtime.register_extension(otter_sql::sql_extension())?;
    runtime.register_extension(otter_kv::kv_extension())?;

    // Build environment from system (standalone has full access)
    // Pass through all environment variables
    let mut env_builder = EnvStoreBuilder::new();
    for (key, _) in std::env::vars() {
        env_builder = env_builder.passthrough_var(key);
    }
    let env_store = Arc::new(env_builder.build());

    // Create process info
    let args: Vec<String> = std::env::args().collect();
    let process_info = ProcessInfo::new(env_store, args.clone());

    // Set up globals
    let args_json = serde_json::to_string(&args)?;
    let process_setup = process_info.to_js_setup();
    let setup = format!(
        "{process_setup}\n\
         globalThis.__otter_lock_builtins && globalThis.__otter_lock_builtins();\n\
         globalThis.Otter = globalThis.Otter || {{}};\n\
         globalThis.Otter.args = {args_json};\n"
    );

    // Execute with error handling
    let wrapped = format!(
        "{setup}\n\
         globalThis.__otter_script_error = null;\n\
         (async () => {{\n\
           try {{\n\
             {code}\n\
           }} catch (err) {{\n\
             globalThis.__otter_script_error = err ? String(err) : 'Error';\n\
           }}\n\
         }})();\n"
    );

    runtime.eval(&wrapped)?;

    // Run event loop (no timeout for standalone executables)
    runtime.run_event_loop_until_idle(Duration::ZERO)?;

    // Check for errors
    let error = runtime.context().get_global("__otter_script_error")?;
    if !error.is_null() && !error.is_undefined() {
        return Err(anyhow::anyhow!("{}", error.to_string()?));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_embedded_no_trailer() {
        let binary = vec![0u8; 100];
        let result = strip_embedded(&binary);
        assert_eq!(result, binary);
    }

    #[test]
    fn test_strip_embedded_with_trailer() {
        let mut binary = vec![0u8; 100];
        let code = b"console.log('hello')";
        let offset = binary.len() as u64;

        binary.extend_from_slice(code);
        binary.extend_from_slice(MAGIC);
        binary.extend_from_slice(&offset.to_le_bytes());

        let result = strip_embedded(&binary);
        assert_eq!(result.len(), 100);
    }

    #[test]
    fn test_trailer_roundtrip() {
        let original = vec![1u8, 2, 3, 4, 5];
        let code = b"test code";

        // Simulate embedding
        let mut with_code = original.clone();
        let offset = with_code.len() as u64;
        with_code.extend_from_slice(code);
        with_code.extend_from_slice(MAGIC);
        with_code.extend_from_slice(&offset.to_le_bytes());

        // Strip should recover original
        let stripped = strip_embedded(&with_code);
        assert_eq!(stripped, original);
    }
}
