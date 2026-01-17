//! Binary lockfile format (otter.lockb)
//!
//! Format:
//! - Header: "OTTER\x00\x01\x00" (8 bytes) - magic + version
//! - Package count: u32 LE
//! - Packages: [PackageEntry; count]
//! - String buffer: concatenated strings

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// Magic bytes for binary lockfile
const MAGIC: &[u8; 6] = b"OTTER\x00";
/// Format version
const VERSION: u16 = 1;

/// Binary lockfile
#[derive(Debug, Clone, Default)]
pub struct BinaryLockfile {
    pub packages: HashMap<String, BinaryLockEntry>,
}

/// Package entry in binary lockfile
#[derive(Debug, Clone)]
pub struct BinaryLockEntry {
    pub version: String,
    pub resolved: String,
    pub integrity: Option<String>,
}

/// Fixed-size package entry for serialization (30 bytes)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
struct PackageEntry {
    /// Name offset in string buffer
    name_offset: u32,
    /// Name length
    name_len: u16,
    /// Version offset in string buffer
    version_offset: u32,
    /// Version length
    version_len: u16,
    /// Resolved URL offset
    resolved_offset: u32,
    /// Resolved URL length
    resolved_len: u16,
    /// Integrity offset (0 if none)
    integrity_offset: u32,
    /// Integrity length (0 if none)
    integrity_len: u16,
    /// Reserved for future use
    _reserved: [u8; 6],
}

impl BinaryLockfile {
    /// Create new empty lockfile
    pub fn new() -> Self {
        Self {
            packages: HashMap::new(),
        }
    }

    /// Load from binary file
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;

        Self::from_bytes(&data)
    }

    /// Parse from bytes
    pub fn from_bytes(data: &[u8]) -> io::Result<Self> {
        if data.len() < 12 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Lockfile too small",
            ));
        }

        // Check magic
        if &data[0..6] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid lockfile magic",
            ));
        }

        // Check version
        let version = u16::from_le_bytes([data[6], data[7]]);
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unsupported lockfile version: {}", version),
            ));
        }

        // Read package count
        let pkg_count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;

        // Calculate offsets
        let entries_start = 12;
        let entries_size = pkg_count * std::mem::size_of::<PackageEntry>();
        let strings_start = entries_start + entries_size;

        if data.len() < strings_start {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Lockfile truncated",
            ));
        }

        let strings_buf = &data[strings_start..];

        // Parse packages
        let mut packages = HashMap::with_capacity(pkg_count);

        for i in 0..pkg_count {
            let entry_offset = entries_start + i * std::mem::size_of::<PackageEntry>();
            let entry_bytes = &data[entry_offset..entry_offset + std::mem::size_of::<PackageEntry>()];

            // Parse entry fields manually (packed struct)
            let name_offset = u32::from_le_bytes([entry_bytes[0], entry_bytes[1], entry_bytes[2], entry_bytes[3]]) as usize;
            let name_len = u16::from_le_bytes([entry_bytes[4], entry_bytes[5]]) as usize;
            let version_offset = u32::from_le_bytes([entry_bytes[6], entry_bytes[7], entry_bytes[8], entry_bytes[9]]) as usize;
            let version_len = u16::from_le_bytes([entry_bytes[10], entry_bytes[11]]) as usize;
            let resolved_offset = u32::from_le_bytes([entry_bytes[12], entry_bytes[13], entry_bytes[14], entry_bytes[15]]) as usize;
            let resolved_len = u16::from_le_bytes([entry_bytes[16], entry_bytes[17]]) as usize;
            let integrity_offset = u32::from_le_bytes([entry_bytes[18], entry_bytes[19], entry_bytes[20], entry_bytes[21]]) as usize;
            let integrity_len = u16::from_le_bytes([entry_bytes[22], entry_bytes[23]]) as usize;

            // Extract strings
            let name = String::from_utf8_lossy(&strings_buf[name_offset..name_offset + name_len]).to_string();
            let version = String::from_utf8_lossy(&strings_buf[version_offset..version_offset + version_len]).to_string();
            let resolved = String::from_utf8_lossy(&strings_buf[resolved_offset..resolved_offset + resolved_len]).to_string();
            let integrity = if integrity_len > 0 {
                Some(String::from_utf8_lossy(&strings_buf[integrity_offset..integrity_offset + integrity_len]).to_string())
            } else {
                None
            };

            packages.insert(
                name,
                BinaryLockEntry {
                    version,
                    resolved,
                    integrity,
                },
            );
        }

        Ok(Self { packages })
    }

    /// Save to binary file
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let data = self.to_bytes();
        std::fs::write(path, data)
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut string_buf = Vec::new();
        let mut entries = Vec::with_capacity(self.packages.len());

        // Build string buffer and entries
        for (name, entry) in &self.packages {
            let name_offset = string_buf.len() as u32;
            string_buf.extend_from_slice(name.as_bytes());
            let name_len = name.len() as u16;

            let version_offset = string_buf.len() as u32;
            string_buf.extend_from_slice(entry.version.as_bytes());
            let version_len = entry.version.len() as u16;

            let resolved_offset = string_buf.len() as u32;
            string_buf.extend_from_slice(entry.resolved.as_bytes());
            let resolved_len = entry.resolved.len() as u16;

            let (integrity_offset, integrity_len) = if let Some(ref integrity) = entry.integrity {
                let offset = string_buf.len() as u32;
                string_buf.extend_from_slice(integrity.as_bytes());
                (offset, integrity.len() as u16)
            } else {
                (0, 0)
            };

            entries.push(PackageEntry {
                name_offset,
                name_len,
                version_offset,
                version_len,
                resolved_offset,
                resolved_len,
                integrity_offset,
                integrity_len,
                _reserved: [0; 6],
            });
        }

        // Calculate total size
        let header_size = 8 + 4; // magic + version + count
        let entries_size = entries.len() * std::mem::size_of::<PackageEntry>();
        let total_size = header_size + entries_size + string_buf.len();

        let mut data = Vec::with_capacity(total_size);

        // Write header
        data.extend_from_slice(MAGIC);
        data.extend_from_slice(&VERSION.to_le_bytes());
        data.extend_from_slice(&(entries.len() as u32).to_le_bytes());

        // Write entries
        for entry in &entries {
            data.extend_from_slice(&entry.name_offset.to_le_bytes());
            data.extend_from_slice(&entry.name_len.to_le_bytes());
            data.extend_from_slice(&entry.version_offset.to_le_bytes());
            data.extend_from_slice(&entry.version_len.to_le_bytes());
            data.extend_from_slice(&entry.resolved_offset.to_le_bytes());
            data.extend_from_slice(&entry.resolved_len.to_le_bytes());
            data.extend_from_slice(&entry.integrity_offset.to_le_bytes());
            data.extend_from_slice(&entry.integrity_len.to_le_bytes());
            data.extend_from_slice(&entry._reserved);
        }

        // Write string buffer
        data.extend_from_slice(&string_buf);

        data
    }

    /// Check if package is locked
    pub fn get(&self, name: &str) -> Option<&BinaryLockEntry> {
        self.packages.get(name)
    }

    /// Get locked version
    pub fn get_version(&self, name: &str) -> Option<&str> {
        self.packages.get(name).map(|e| e.version.as_str())
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    /// Number of packages
    pub fn len(&self) -> usize {
        self.packages.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_lockfile_roundtrip() {
        let mut lockfile = BinaryLockfile::new();
        lockfile.packages.insert(
            "lodash".to_string(),
            BinaryLockEntry {
                version: "4.17.21".to_string(),
                resolved: "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string(),
                integrity: Some("sha512-v2kDE...".to_string()),
            },
        );
        lockfile.packages.insert(
            "@types/node".to_string(),
            BinaryLockEntry {
                version: "18.0.0".to_string(),
                resolved: "https://registry.npmjs.org/@types/node/-/node-18.0.0.tgz".to_string(),
                integrity: None,
            },
        );

        // Serialize
        let data = lockfile.to_bytes();

        // Deserialize
        let loaded = BinaryLockfile::from_bytes(&data).unwrap();

        assert_eq!(loaded.packages.len(), 2);
        assert_eq!(loaded.get_version("lodash"), Some("4.17.21"));
        assert_eq!(loaded.get_version("@types/node"), Some("18.0.0"));
        assert!(loaded.get("lodash").unwrap().integrity.is_some());
        assert!(loaded.get("@types/node").unwrap().integrity.is_none());
    }

    #[test]
    fn test_binary_lockfile_size() {
        assert_eq!(std::mem::size_of::<PackageEntry>(), 30);
    }

    #[test]
    fn test_invalid_magic() {
        let data = b"WRONG\x00\x01\x00\x00\x00\x00\x00";
        let result = BinaryLockfile::from_bytes(data);
        assert!(result.is_err());
    }
}
