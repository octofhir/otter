//! tsgo binary download and cache management.
//!
//! This module handles downloading the tsgo binary from Deno's fork of typescript-go
//! which includes the `--api` RPC mode for programmatic integration.
//!
//! Note: We use Deno's fork because it has the `--api` flag for RPC communication.
//! The official Microsoft `@typescript/native-preview` does not have this feature yet.
//!
//! See: https://github.com/denoland/typescript-go

use crate::error::{JscError, JscResult};
use std::path::PathBuf;

/// Current version of tsgo to download (from Deno's fork).
const TSGO_VERSION: &str = "0.1.15";

/// GitHub release URL for Deno's typescript-go fork.
const DOWNLOAD_BASE_URL: &str = "https://github.com/denoland/typescript-go/releases/download";

/// Get platform identifier for binary download.
///
/// Returns the platform string used in tsgo release artifacts.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn get_platform() -> &'static str {
    "macos-arm64"
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn get_platform() -> &'static str {
    "macos-x64"
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn get_platform() -> &'static str {
    "linux-x64"
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn get_platform() -> &'static str {
    "linux-arm64"
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn get_platform() -> &'static str {
    "windows-x64"
}

#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
    all(target_os = "windows", target_arch = "x86_64"),
)))]
fn get_platform() -> &'static str {
    compile_error!("Unsupported platform for tsgo")
}

/// Get the cache directory for tsgo binaries.
///
/// Returns: `~/.cache/otter/tsgo/v{version}/` on Unix,
/// or appropriate cache location on other platforms.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("otter")
        .join("tsgo")
        .join(format!("v{}", TSGO_VERSION))
}

/// Get the expected binary name for the current platform.
fn binary_name() -> &'static str {
    #[cfg(windows)]
    {
        "tsgo.exe"
    }
    #[cfg(not(windows))]
    {
        "tsgo"
    }
}

/// Get the full binary path including platform suffix.
fn platform_binary_name() -> String {
    let platform = get_platform();
    #[cfg(windows)]
    {
        format!("tsgo-{}.exe", platform)
    }
    #[cfg(not(windows))]
    {
        format!("tsgo-{}", platform)
    }
}

/// Find tsgo binary - checks cache, PATH, then downloads if needed.
///
/// Search order:
/// 1. Cached binary in `~/.cache/otter/tsgo/v{version}/`
/// 2. `tsgo` in PATH (allows user-installed version)
/// 3. Auto-download from GitHub releases
///
/// # Errors
///
/// Returns error if binary cannot be found or downloaded.
pub async fn find_tsgo() -> JscResult<PathBuf> {
    // Check cache first (platform-specific name like tsgo-macos-arm64)
    let platform_name = platform_binary_name();
    let cached = cache_dir().join(&platform_name);
    if cached.exists() {
        tracing::debug!("Found cached tsgo at {:?}", cached);
        return Ok(cached);
    }

    // Check PATH for user-installed version
    if let Ok(path) = which::which("tsgo") {
        tracing::debug!("Found tsgo in PATH at {:?}", path);
        return Ok(path);
    }

    // Download if not found
    tracing::info!("tsgo not found, downloading...");
    download_tsgo().await
}

/// Find tsgo binary synchronously (blocking).
///
/// Same as `find_tsgo` but blocks the current thread.
pub fn find_tsgo_blocking() -> JscResult<PathBuf> {
    // Check cache first
    let platform_name = platform_binary_name();
    let cached = cache_dir().join(&platform_name);
    if cached.exists() {
        return Ok(cached);
    }

    // Check PATH
    if let Ok(path) = which::which("tsgo") {
        return Ok(path);
    }

    // Download if not found (blocking)
    download_tsgo_blocking()
}

/// Downloads the platform-specific binary from GitHub releases,
/// extracts it from the zip archive, and caches it for future use.
pub async fn download_tsgo() -> JscResult<PathBuf> {
    let platform = get_platform();
    let url = format!(
        "{}/v{}/typescript-go-{}-{}.zip",
        DOWNLOAD_BASE_URL, TSGO_VERSION, TSGO_VERSION, platform
    );

    tracing::info!("Downloading tsgo from {}", url);

    let dest = cache_dir();
    std::fs::create_dir_all(&dest).map_err(|e| {
        JscError::internal(format!(
            "Failed to create cache directory {:?}: {}",
            dest, e
        ))
    })?;

    // Download
    let response = reqwest::get(&url)
        .await
        .map_err(|e| JscError::internal(format!("Failed to download tsgo: {}", e)))?;

    if !response.status().is_success() {
        return Err(JscError::internal(format!(
            "Failed to download tsgo: HTTP {}",
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| JscError::internal(format!("Failed to read response body: {}", e)))?;

    extract_zip(&bytes, &dest)?;

    let platform_name = platform_binary_name();
    let binary_path = dest.join(&platform_name);

    // Verify binary exists
    if !binary_path.exists() {
        // Try the simple binary name
        let simple_path = dest.join(binary_name());
        if simple_path.exists() {
            // Rename to platform-specific name
            std::fs::rename(&simple_path, &binary_path)
                .map_err(|e| JscError::internal(format!("Failed to rename binary: {}", e)))?;
        } else {
            return Err(JscError::internal(format!(
                "Downloaded archive did not contain expected binary at: {:?}",
                binary_path
            )));
        }
    }

    // Make executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755)).map_err(
            |e| JscError::internal(format!("Failed to set executable permission: {}", e)),
        )?;
    }

    tracing::info!("Successfully downloaded tsgo to {:?}", binary_path);
    Ok(binary_path)
}

/// Download tsgo binary synchronously (blocking).
pub fn download_tsgo_blocking() -> JscResult<PathBuf> {
    let platform = get_platform();
    let url = format!(
        "{}/v{}/typescript-go-{}-{}.zip",
        DOWNLOAD_BASE_URL, TSGO_VERSION, TSGO_VERSION, platform
    );

    let dest = cache_dir();
    std::fs::create_dir_all(&dest).map_err(|e| {
        JscError::internal(format!(
            "Failed to create cache directory {:?}: {}",
            dest, e
        ))
    })?;

    // Download using blocking reqwest
    let response = reqwest::blocking::get(&url)
        .map_err(|e| JscError::internal(format!("Failed to download tsgo: {}", e)))?;

    if !response.status().is_success() {
        return Err(JscError::internal(format!(
            "Failed to download tsgo: HTTP {}",
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .map_err(|e| JscError::internal(format!("Failed to read response body: {}", e)))?;

    extract_zip(&bytes, &dest)?;

    let platform_name = platform_binary_name();
    let binary_path = dest.join(&platform_name);

    if !binary_path.exists() {
        let simple_path = dest.join(binary_name());
        if simple_path.exists() {
            std::fs::rename(&simple_path, &binary_path)
                .map_err(|e| JscError::internal(format!("Failed to rename binary: {}", e)))?;
        } else {
            return Err(JscError::internal(format!(
                "Downloaded archive did not contain expected binary at: {:?}",
                binary_path
            )));
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755)).map_err(
            |e| JscError::internal(format!("Failed to set executable permission: {}", e)),
        )?;
    }

    Ok(binary_path)
}

/// Extract a zip archive to a destination directory.
fn extract_zip(bytes: &[u8], dest: &std::path::Path) -> JscResult<()> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| JscError::internal(format!("Failed to open zip archive: {}", e)))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| JscError::internal(format!("Failed to read zip entry: {}", e)))?;

        let outpath = match file.enclosed_name() {
            Some(path) => dest.join(path),
            None => continue,
        };

        if file.name().ends_with('/') {
            std::fs::create_dir_all(&outpath).map_err(|e| {
                JscError::internal(format!("Failed to create directory {:?}: {}", outpath, e))
            })?;
        } else {
            if let Some(p) = outpath.parent()
                && !p.exists()
            {
                std::fs::create_dir_all(p).map_err(|e| {
                    JscError::internal(format!("Failed to create directory {:?}: {}", p, e))
                })?;
            }
            let mut outfile = std::fs::File::create(&outpath).map_err(|e| {
                JscError::internal(format!("Failed to create file {:?}: {}", outpath, e))
            })?;
            std::io::copy(&mut file, &mut outfile).map_err(|e| {
                JscError::internal(format!("Failed to write file {:?}: {}", outpath, e))
            })?;
        }

        // Set permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = file.unix_mode() {
                std::fs::set_permissions(&outpath, std::fs::Permissions::from_mode(mode)).ok();
            }
        }
    }

    Ok(())
}

/// Check if tsgo is available (cached or in PATH).
///
/// This is a quick check that doesn't trigger downloads.
pub fn is_tsgo_available() -> bool {
    let platform_name = platform_binary_name();
    let cached = cache_dir().join(&platform_name);
    cached.exists() || which::which("tsgo").is_ok()
}

/// Get the currently configured tsgo version.
pub fn tsgo_version() -> &'static str {
    TSGO_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_dir() {
        let dir = cache_dir();
        assert!(dir.to_string_lossy().contains("otter"));
        assert!(dir.to_string_lossy().contains("tsgo"));
    }

    #[test]
    fn test_get_platform() {
        let platform = get_platform();
        // Should be one of the supported platforms
        assert!(
            platform == "macos-arm64"
                || platform == "macos-x64"
                || platform == "linux-x64"
                || platform == "linux-arm64"
                || platform == "windows-x64"
        );
    }

    #[test]
    fn test_tsgo_version() {
        assert!(!tsgo_version().is_empty());
    }
}
