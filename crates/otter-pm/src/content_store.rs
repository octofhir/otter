//! Content-Addressable Storage (CAS) for package files
//!
//! Files are stored by their SHA256 hash, enabling deduplication
//! and instant installs via clonefile (macOS) or hardlinks.

use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::{collections::HashSet, io::ErrorKind};

/// Try to use clonefile on macOS (copy-on-write, instant)
#[cfg(target_os = "macos")]
fn try_clonefile(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src_cstr = CString::new(src.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let dst_cstr = CString::new(dst.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    // clonefile(src, dst, 0) - CLONE_NOFOLLOW is 0x0001 but we use 0 for default
    let result = unsafe { libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), 0) };

    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn try_clonefile(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clonefile not available",
    ))
}

#[cfg(target_os = "macos")]
fn try_copyfile_clone_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn copyfile(
            from: *const libc::c_char,
            to: *const libc::c_char,
            state: *mut libc::c_void,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }

    // Keep these aligned with <copyfile.h>.
    // We intentionally do NOT pass COPYFILE_STAT/COPYFILE_ACL/COPYFILE_XATTR here, because that
    // explodes syscall counts (xattr/acl churn) and isn't needed for node_modules trees.
    const COPYFILE_DATA: libc::c_uint = 1 << 3;
    const COPYFILE_RECURSIVE: libc::c_uint = 1 << 15;
    const COPYFILE_CLONE: libc::c_uint = 1 << 24;

    let src_cstr = CString::new(src.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let dst_cstr = CString::new(dst.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let flags = COPYFILE_DATA | COPYFILE_RECURSIVE | COPYFILE_CLONE;
    let result = unsafe {
        copyfile(
            src_cstr.as_ptr(),
            dst_cstr.as_ptr(),
            std::ptr::null_mut(),
            flags,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Public wrapper for try_clonefile (used by install.rs)
pub fn try_clonefile_pub(src: &Path, dst: &Path) -> io::Result<()> {
    try_clonefile(src, dst)
}

/// Content-addressable store for package files
#[derive(Clone)]
pub struct ContentStore {
    /// Root directory for the store (~/.cache/otter/store)
    store_dir: PathBuf,
    /// Index directory for package file mappings
    index_dir: PathBuf,
    /// Directory with assembled package trees (~/.cache/otter/pkgs)
    package_dir: PathBuf,
}

/// Stored file info
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredFile {
    /// SHA256 hash of file content
    pub hash: String,
    /// Relative path within the package
    pub path: String,
    /// File mode (permissions)
    pub mode: u32,
}

/// Package file index
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackageIndex {
    pub name: String,
    pub version: String,
    pub files: Vec<StoredFile>,
}

impl ContentStore {
    /// Create a new content store
    pub fn new() -> Self {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("otter");

        Self {
            store_dir: cache_dir.join("store"),
            index_dir: cache_dir.join("index"),
            package_dir: cache_dir.join("pkgs"),
        }
    }

    /// Get the store directory path
    pub fn store_path(&self) -> &Path {
        &self.store_dir
    }

    /// Check if a package is already in the store
    pub fn has_package(&self, name: &str, version: &str) -> bool {
        self.index_path(name, version).exists()
    }

    /// Get package index if it exists
    pub fn get_package_index(&self, name: &str, version: &str) -> Option<PackageIndex> {
        let path = self.index_path(name, version);
        let data = fs::read(&path).ok()?;
        let index: PackageIndex = serde_json::from_slice(&data).ok()?;
        if !index.files.iter().any(|f| f.path == "package.json") {
            return None;
        }
        Some(index)
    }

    fn package_store_path(&self, name: &str, version: &str) -> PathBuf {
        let safe_name = name.replace('/', "-").replace('@', "");
        self.package_dir.join(safe_name).join(version)
    }

    /// Store a file and return its hash
    pub fn store_file(&self, content: &[u8]) -> io::Result<String> {
        let hash = Self::hash_content(content);
        let store_path = self.content_path(&hash);

        // Already stored?
        if store_path.exists() {
            return Ok(hash);
        }

        // Create parent directory (first 2 chars of hash)
        if let Some(parent) = store_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("Failed to create store dir {:?}: {}", parent, e),
                )
            })?;
        }

        // Write atomically via temp file with unique name (PID + random)
        let temp_path = store_path.with_extension(format!(
            "tmp.{}.{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));

        fs::write(&temp_path, content).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to write temp file {:?}: {}", temp_path, e),
            )
        })?;

        // Rename is atomic on same filesystem; if target exists, another thread won
        match fs::rename(&temp_path, &store_path) {
            Ok(()) => {}
            Err(_) if store_path.exists() => {
                // Another thread already stored it, remove our temp file
                let _ = fs::remove_file(&temp_path);
            }
            Err(e) => {
                let _ = fs::remove_file(&temp_path);
                return Err(io::Error::new(
                    e.kind(),
                    format!(
                        "Failed to rename {:?} to {:?}: {}",
                        temp_path, store_path, e
                    ),
                ));
            }
        }

        Ok(hash)
    }

    /// Create a file from store to destination using clonefile (macOS), hardlink, or copy
    pub fn link_file(&self, hash: &str, dest: &Path, mode: u32) -> io::Result<()> {
        let store_path = self.content_path(hash);

        if !store_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Content not in store: {} (path: {:?})", hash, store_path),
            ));
        }

        // Create parent directories
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("Failed to create dest parent {:?}: {}", parent, e),
                )
            })?;
        }

        // Remove existing file if any
        if dest.exists() {
            fs::remove_file(dest).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("Failed to remove existing file {:?}: {}", dest, e),
                )
            })?;
        }

        // Try clonefile (macOS CoW - fastest), then hardlink, then copy
        // clonefile and hardlink preserve permissions, only copy needs chmod
        let needs_chmod = if try_clonefile(&store_path, dest).is_ok() {
            false // clonefile preserves permissions
        } else if fs::hard_link(&store_path, dest).is_ok() {
            false // hardlink shares inode, same permissions
        } else {
            // Hardlink failed (cross-device?), copy instead
            fs::copy(&store_path, dest).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("Failed to copy {:?} to {:?}: {}", store_path, dest, e),
                )
            })?;
            true
        };

        // Only set permissions if we used copy
        #[cfg(unix)]
        if needs_chmod {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(mode);
            fs::set_permissions(dest, perms).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("Failed to set permissions on {:?}: {}", dest, e),
                )
            })?;
        }

        Ok(())
    }

    /// Save package index
    pub fn save_package_index(&self, index: &PackageIndex) -> io::Result<()> {
        let path = self.index_path(&index.name, &index.version);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json =
            serde_json::to_vec(index).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&path, json)?;

        Ok(())
    }

    /// Install package from store using clonefile/hardlinks (parallel with rayon)
    pub fn install_from_store(&self, name: &str, version: &str, dest: &Path) -> io::Result<bool> {
        let Some(index) = self.get_package_index(name, version) else {
            return Ok(false);
        };

        self.install_from_index(&index, dest)?;
        Ok(true)
    }

    pub fn install_from_index(&self, index: &PackageIndex, dest: &Path) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            if self
                .install_from_existing_package_store(index, dest)
                .is_ok()
            {
                return Ok(());
            }

            // If the assembled tree is missing but we have the CAS index, build it once and retry.
            if self.maybe_build_package_store_from_index(index).is_ok() {
                if self
                    .install_from_existing_package_store(index, dest)
                    .is_ok()
                {
                    return Ok(());
                }
            }
        }

        self.install_from_index_files(index, dest)?;

        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn validate_installed_tree_against_index(
        &self,
        index: &PackageIndex,
        root: &Path,
    ) -> io::Result<()> {
        let mut candidates: Vec<(&str, u32)> = Vec::new();

        for file in &index.files {
            let rel = file.path.trim_start_matches('/');
            if rel == "package.json" {
                candidates.push((rel, file.mode));
                break;
            }
        }

        if let Some(exec_file) = index
            .files
            .iter()
            .find(|f| (f.mode & 0o111) != 0)
            .map(|f| (f.path.trim_start_matches('/'), f.mode))
        {
            candidates.push(exec_file);
        }

        if candidates.is_empty() {
            if let Some(first) = index.files.first() {
                candidates.push((first.path.trim_start_matches('/'), first.mode));
            }
        }

        for (rel, expected_mode) in candidates.into_iter().take(2) {
            let p = root.join(rel);
            let actual_mode = fs::metadata(&p)?.permissions().mode() & 0o777;
            let expected_mode = expected_mode & 0o777;
            if actual_mode != expected_mode {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "cloned tree permissions mismatch for {:?}: expected {:o}, got {:o}",
                        p, expected_mode, actual_mode
                    ),
                ));
            }
        }

        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn install_from_existing_package_store(
        &self,
        index: &PackageIndex,
        dest: &Path,
    ) -> io::Result<()> {
        let pkg_dir = self.package_store_path(&index.name, &index.version);
        if !pkg_dir.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Package store dir missing",
            ));
        }

        if dest.exists() {
            fs::remove_dir_all(dest)?;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        // Try clonefile() on the package directory first. While Apple discourages directory cloning
        // via clonefile(2), it's often much cheaper than userland copyfile(3) recursion and matches
        // what fast installers tend to do on APFS. Fall back to copyfile if it fails.
        if try_clonefile(&pkg_dir, dest).is_ok() {
            self.validate_installed_tree_against_index(index, dest)?;
            return Ok(());
        }

        try_copyfile_clone_recursive(&pkg_dir, dest)?;
        self.validate_installed_tree_against_index(index, dest)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn maybe_build_package_store_from_index(&self, index: &PackageIndex) -> io::Result<()> {
        let pkg_dir = self.package_store_path(&index.name, &index.version);
        if pkg_dir.exists() {
            return Ok(());
        }

        let pkg_parent = pkg_dir
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Invalid package store path"))?;
        fs::create_dir_all(pkg_parent)?;

        let temp_dir = pkg_parent.join(format!(
            "{}.tmp.{}.{:x}",
            index.version,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));

        self.install_from_index_files(index, &temp_dir)?;

        match fs::rename(&temp_dir, &pkg_dir) {
            Ok(()) => Ok(()),
            Err(_) if pkg_dir.exists() => {
                let _ = fs::remove_dir_all(&temp_dir);
                Ok(())
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&temp_dir);
                Err(e)
            }
        }
    }

    fn install_from_index_files(&self, index: &PackageIndex, dest: &Path) -> io::Result<()> {
        // Remove existing package dir
        if dest.exists() {
            fs::remove_dir_all(dest)?;
        }

        // Create destination directory (and parents for scoped packages like @types/node)
        fs::create_dir_all(dest)?;

        // Pre-create all subdirectories (avoid races in parallel execution)
        let mut dirs_to_create: HashSet<PathBuf> = HashSet::new();
        for file in &index.files {
            let Some(parent) = Path::new(&file.path).parent() else {
                continue;
            };
            if parent.as_os_str().is_empty() {
                continue;
            }

            let mut current = PathBuf::new();
            for component in parent.components() {
                current.push(component.as_os_str());
                dirs_to_create.insert(dest.join(&current));
            }
        }

        let mut dirs: Vec<PathBuf> = dirs_to_create.into_iter().collect();
        dirs.sort_by_key(|p| p.components().count());

        for dir in dirs {
            match fs::create_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }

        // Link all files in parallel using rayon
        // Skip exists() check - let clonefile/hardlink/copy fail if file missing
        let store_dir = &self.store_dir;
        let results: Vec<io::Result<()>> = index
            .files
            .par_iter()
            .map(|file| {
                let file_dest = dest.join(&file.path);
                let hash = &file.hash;
                let prefix = &hash[..2.min(hash.len())];
                let store_path = store_dir.join(prefix).join(hash);

                // Try clonefile, then hardlink, then copy
                // clonefile and hardlink preserve permissions, only copy needs chmod
                let needs_chmod = if try_clonefile(&store_path, &file_dest).is_ok() {
                    false // clonefile preserves permissions
                } else if fs::hard_link(&store_path, &file_dest).is_ok() {
                    false // hardlink shares inode, same permissions
                } else {
                    fs::copy(&store_path, &file_dest).map_err(|e| {
                        io::Error::new(
                            e.kind(),
                            format!("Content not in store: {} ({:?})", hash, store_path),
                        )
                    })?;
                    true // copy may not preserve permissions
                };

                // Only set permissions if we used copy
                #[cfg(unix)]
                if needs_chmod {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = fs::Permissions::from_mode(file.mode);
                    fs::set_permissions(&file_dest, perms)?;
                }

                Ok(())
            })
            .collect();

        // Check for errors
        for result in results {
            result?;
        }

        Ok(())
    }

    /// Store package files from tarball and create index
    pub fn store_package_from_tarball(
        &self,
        name: &str,
        version: &str,
        tarball: &[u8],
    ) -> io::Result<PackageIndex> {
        use flate2::read::GzDecoder;
        use tar::Archive;

        let gz = GzDecoder::new(tarball);
        let mut archive = Archive::new(gz);

        let mut files = Vec::new();

        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?;

            // Skip directories
            if entry.header().entry_type().is_dir() {
                continue;
            }

            // npm tarballs contain a single top-level directory (often "package/", but not always).
            // Strip the first path component so files install into the package root.
            let rel_path: PathBuf = path.iter().skip(1).collect();
            let rel_path = rel_path.to_string_lossy().to_string();

            if rel_path.is_empty() {
                continue;
            }

            // Read content
            let mut content = Vec::new();
            entry.read_to_end(&mut content)?;

            // Store in CAS
            let hash = self.store_file(&content)?;

            // Get file mode
            let mode = entry.header().mode().unwrap_or(0o644);

            files.push(StoredFile {
                hash,
                path: rel_path,
                mode,
            });
        }

        let index = PackageIndex {
            name: name.to_string(),
            version: version.to_string(),
            files,
        };

        // Save index
        self.save_package_index(&index)?;

        Ok(index)
    }

    /// Hash content using SHA256
    fn hash_content(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        let result = hasher.finalize();
        hex::encode(result)
    }

    /// Get path in store for a given hash
    fn content_path(&self, hash: &str) -> PathBuf {
        // Use first 2 chars as subdirectory for better filesystem performance
        let prefix = &hash[..2.min(hash.len())];
        self.store_dir.join(prefix).join(hash)
    }

    /// Get index path for a package
    fn index_path(&self, name: &str, version: &str) -> PathBuf {
        let safe_name = name.replace('/', "-").replace('@', "");
        self.index_dir
            .join(format!("{}-{}.json", safe_name, version))
    }

    /// Get store statistics
    pub fn stats(&self) -> io::Result<StoreStats> {
        let mut file_count = 0u64;
        let mut total_size = 0u64;

        if self.store_dir.exists() {
            for entry in walkdir::WalkDir::new(&self.store_dir)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if entry.file_type().is_file() {
                    file_count += 1;
                    total_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }

        let mut package_count = 0u64;
        if self.index_dir.exists() {
            for entry in fs::read_dir(&self.index_dir)? {
                if entry?
                    .path()
                    .extension()
                    .map(|e| e == "json")
                    .unwrap_or(false)
                {
                    package_count += 1;
                }
            }
        }

        Ok(StoreStats {
            file_count,
            total_size,
            package_count,
        })
    }
}

impl Default for ContentStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Store statistics
#[derive(Debug)]
pub struct StoreStats {
    pub file_count: u64,
    pub total_size: u64,
    pub package_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    use tar::Builder;

    #[test]
    fn test_hash_content() {
        let hash = ContentStore::hash_content(b"hello world");
        assert_eq!(hash.len(), 64); // SHA256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn test_content_path() {
        let store = ContentStore::new();
        let hash = "abcdef1234567890";
        let path = store.content_path(hash);
        assert!(path.to_string_lossy().contains("/ab/"));
        assert!(path.to_string_lossy().ends_with(hash));
    }

    #[test]
    fn test_store_and_link() {
        let store = ContentStore {
            store_dir: PathBuf::from("/tmp/otter-test-store"),
            index_dir: PathBuf::from("/tmp/otter-test-index"),
            package_dir: PathBuf::from("/tmp/otter-test-pkgs"),
        };

        // Clean up
        let _ = fs::remove_dir_all("/tmp/otter-test-store");
        let _ = fs::remove_dir_all("/tmp/otter-test-index");
        let _ = fs::remove_dir_all("/tmp/otter-test-pkgs");

        // Store a file
        let content = b"test content";
        let hash = store.store_file(content).unwrap();

        // Link it somewhere
        let dest = PathBuf::from("/tmp/otter-test-link");
        store.link_file(&hash, &dest, 0o644).unwrap();

        // Verify content
        let read_content = fs::read(&dest).unwrap();
        assert_eq!(read_content, content);

        // Clean up
        let _ = fs::remove_dir_all("/tmp/otter-test-store");
        let _ = fs::remove_dir_all("/tmp/otter-test-index");
        let _ = fs::remove_dir_all("/tmp/otter-test-pkgs");
        let _ = fs::remove_file("/tmp/otter-test-link");
    }

    #[test]
    fn test_store_package_strips_top_level_dir() {
        fn make_tgz(top_level: &str) -> Vec<u8> {
            let mut tar_buf = Vec::new();
            {
                let gz = GzEncoder::new(&mut tar_buf, Compression::default());
                let mut tar = Builder::new(gz);

                let mut header = tar::Header::new_gnu();
                let contents = br#"{ "name": "x", "version": "1.0.0" }"#;
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(
                    &mut header,
                    format!("{}/package.json", top_level),
                    &contents[..],
                )
                .unwrap();
                tar.finish().unwrap();
            }
            tar_buf
        }

        let store_root = PathBuf::from(format!("/tmp/otter-test-store-{}", std::process::id()));
        let store = ContentStore {
            store_dir: store_root.join("store"),
            index_dir: store_root.join("index"),
            package_dir: store_root.join("pkgs"),
        };

        let _ = fs::remove_dir_all(&store_root);

        let tgz = make_tgz("package");
        let index = store
            .store_package_from_tarball("x", "1.0.0", &tgz)
            .unwrap();
        assert!(index.files.iter().any(|f| f.path == "package.json"));

        let tgz = make_tgz("node");
        let index = store
            .store_package_from_tarball("y", "1.0.0", &tgz)
            .unwrap();
        assert!(index.files.iter().any(|f| f.path == "package.json"));
        assert!(!index.files.iter().any(|f| f.path == "node/package.json"));

        let _ = fs::remove_dir_all(&store_root);
    }
}
