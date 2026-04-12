//! Startup snapshot — persist compiled bytecode and profile data for fast cold start.
//!
//! ## Design
//!
//! A snapshot bundles:
//! 1. **Bytecode cache**: compiled bytecode for builtins/bootstrap JS
//! 2. **Profile cache**: FeedbackVector data from previous runs
//! 3. **Version tag**: ABI version + engine version for invalidation
//!
//! On cold start, the runtime checks if a valid snapshot exists:
//! - If valid: skip parse + compile, load bytecode directly
//! - If invalid (version mismatch, missing): normal cold start
//!
//! ## Safety
//!
//! Cache hits must NEVER compromise correctness. Invalidation is conservative:
//! any mismatch in engine version, ISA, feature flags → full recompile.
//!
//! V8: startup snapshots + bytecode caching
//! Hermes: precompiled bytecode
//! Deno: custom runtime snapshots
//!
//! Spec: Phase 8.1 of JIT_INCREMENTAL_PLAN.md

use std::io::{self, Read, Write};

/// Magic bytes identifying an Otter snapshot file.
const SNAPSHOT_MAGIC: &[u8; 8] = b"OTTRSNAP";

/// Current snapshot format version. Increment on any breaking change.
const SNAPSHOT_VERSION: u32 = 1;

/// Snapshot header — stored at the beginning of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// Magic bytes.
    pub magic: [u8; 8],
    /// Snapshot format version.
    pub version: u32,
    /// Engine ABI version (from otter-vm).
    pub abi_version: u32,
    /// Target architecture (0 = x86-64, 1 = aarch64, 2 = other).
    pub arch: u8,
    /// Number of bytecode entries.
    pub bytecode_count: u32,
    /// Number of profile entries.
    pub profile_count: u32,
    /// SHA-256 hash of the source files (for invalidation).
    pub source_hash: [u8; 32],
}

impl SnapshotHeader {
    /// Create a header for the current platform.
    #[must_use]
    pub fn new(bytecode_count: u32, profile_count: u32, source_hash: [u8; 32]) -> Self {
        Self {
            magic: *SNAPSHOT_MAGIC,
            version: SNAPSHOT_VERSION,
            abi_version: 1, // VmAbiVersion::V1
            arch: current_arch(),
            bytecode_count,
            profile_count,
            source_hash,
        }
    }

    /// Validate that a header is compatible with the current engine.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.magic == *SNAPSHOT_MAGIC
            && self.version == SNAPSHOT_VERSION
            && self.abi_version == 1
            && self.arch == current_arch()
    }

    /// Serialize the header to bytes.
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.magic)?;
        w.write_all(&self.version.to_le_bytes())?;
        w.write_all(&self.abi_version.to_le_bytes())?;
        w.write_all(&[self.arch])?;
        w.write_all(&self.bytecode_count.to_le_bytes())?;
        w.write_all(&self.profile_count.to_le_bytes())?;
        w.write_all(&self.source_hash)?;
        Ok(())
    }

    /// Deserialize a header from bytes.
    pub fn read_from(r: &mut impl Read) -> io::Result<Self> {
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;

        let mut buf4 = [0u8; 4];
        r.read_exact(&mut buf4)?;
        let version = u32::from_le_bytes(buf4);

        r.read_exact(&mut buf4)?;
        let abi_version = u32::from_le_bytes(buf4);

        let mut arch_buf = [0u8; 1];
        r.read_exact(&mut arch_buf)?;
        let arch = arch_buf[0];

        r.read_exact(&mut buf4)?;
        let bytecode_count = u32::from_le_bytes(buf4);

        r.read_exact(&mut buf4)?;
        let profile_count = u32::from_le_bytes(buf4);

        let mut source_hash = [0u8; 32];
        r.read_exact(&mut source_hash)?;

        Ok(Self {
            magic,
            version,
            abi_version,
            arch,
            bytecode_count,
            profile_count,
            source_hash,
        })
    }
}

/// A cached bytecode entry.
#[derive(Debug, Clone)]
pub struct BytecodeEntry {
    /// Function name.
    pub name: String,
    /// Source file path.
    pub source_path: String,
    /// Serialized bytecode bytes.
    pub bytecode: Vec<u8>,
}

/// A cached profile entry (feedback vector snapshot).
#[derive(Debug, Clone)]
pub struct ProfileEntry {
    /// Function name this profile belongs to.
    pub function_name: String,
    /// Serialized feedback vector bytes.
    pub feedback_data: Vec<u8>,
}

/// A complete snapshot file.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub header: SnapshotHeader,
    pub bytecodes: Vec<BytecodeEntry>,
    pub profiles: Vec<ProfileEntry>,
}

impl Snapshot {
    /// Create an empty snapshot.
    #[must_use]
    pub fn new(source_hash: [u8; 32]) -> Self {
        Self {
            header: SnapshotHeader::new(0, 0, source_hash),
            bytecodes: Vec::new(),
            profiles: Vec::new(),
        }
    }

    /// Add a bytecode entry.
    pub fn add_bytecode(&mut self, entry: BytecodeEntry) {
        self.bytecodes.push(entry);
        self.header.bytecode_count = self.bytecodes.len() as u32;
    }

    /// Add a profile entry.
    pub fn add_profile(&mut self, entry: ProfileEntry) {
        self.profiles.push(entry);
        self.header.profile_count = self.profiles.len() as u32;
    }

    /// Serialize the snapshot to a writer.
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        self.header.write_to(w)?;

        // Bytecode entries.
        for entry in &self.bytecodes {
            write_length_prefixed_string(w, &entry.name)?;
            write_length_prefixed_string(w, &entry.source_path)?;
            write_length_prefixed_bytes(w, &entry.bytecode)?;
        }

        // Profile entries.
        for entry in &self.profiles {
            write_length_prefixed_string(w, &entry.function_name)?;
            write_length_prefixed_bytes(w, &entry.feedback_data)?;
        }

        Ok(())
    }

    /// Deserialize a snapshot from a reader.
    pub fn read_from(r: &mut impl Read) -> io::Result<Self> {
        let header = SnapshotHeader::read_from(r)?;

        let mut bytecodes = Vec::with_capacity(header.bytecode_count as usize);
        for _ in 0..header.bytecode_count {
            let name = read_length_prefixed_string(r)?;
            let source_path = read_length_prefixed_string(r)?;
            let bytecode = read_length_prefixed_bytes(r)?;
            bytecodes.push(BytecodeEntry { name, source_path, bytecode });
        }

        let mut profiles = Vec::with_capacity(header.profile_count as usize);
        for _ in 0..header.profile_count {
            let function_name = read_length_prefixed_string(r)?;
            let feedback_data = read_length_prefixed_bytes(r)?;
            profiles.push(ProfileEntry { function_name, feedback_data });
        }

        Ok(Self { header, bytecodes, profiles })
    }
}

fn current_arch() -> u8 {
    #[cfg(target_arch = "x86_64")]
    { 0 }
    #[cfg(target_arch = "aarch64")]
    { 1 }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    { 2 }
}

fn write_length_prefixed_string(w: &mut impl Write, s: &str) -> io::Result<()> {
    let len = s.len() as u32;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(s.as_bytes())?;
    Ok(())
}

fn write_length_prefixed_bytes(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    let len = data.len() as u32;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(data)?;
    Ok(())
}

fn read_length_prefixed_string(r: &mut impl Read) -> io::Result<String> {
    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4)?;
    let len = u32::from_le_bytes(buf4) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn read_length_prefixed_bytes(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4)?;
    let len = u32::from_le_bytes(buf4) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_roundtrip() {
        let header = SnapshotHeader::new(3, 2, [0xAB; 32]);
        assert!(header.is_valid());

        let mut buf = Vec::new();
        header.write_to(&mut buf).unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let decoded = SnapshotHeader::read_from(&mut cursor).unwrap();
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_invalid_header() {
        let mut header = SnapshotHeader::new(0, 0, [0; 32]);
        header.version = 999; // Wrong version.
        assert!(!header.is_valid());
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let mut snap = Snapshot::new([0x42; 32]);
        snap.add_bytecode(BytecodeEntry {
            name: "main".into(),
            source_path: "test.js".into(),
            bytecode: vec![0x01, 0x02, 0x03],
        });
        snap.add_profile(ProfileEntry {
            function_name: "hot_fn".into(),
            feedback_data: vec![0xFF, 0xFE],
        });

        let mut buf = Vec::new();
        snap.write_to(&mut buf).unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let decoded = Snapshot::read_from(&mut cursor).unwrap();

        assert_eq!(decoded.header.bytecode_count, 1);
        assert_eq!(decoded.header.profile_count, 1);
        assert_eq!(decoded.bytecodes[0].name, "main");
        assert_eq!(decoded.bytecodes[0].bytecode, vec![0x01, 0x02, 0x03]);
        assert_eq!(decoded.profiles[0].function_name, "hot_fn");
    }

    #[test]
    fn test_empty_snapshot_roundtrip() {
        let snap = Snapshot::new([0; 32]);
        let mut buf = Vec::new();
        snap.write_to(&mut buf).unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let decoded = Snapshot::read_from(&mut cursor).unwrap();
        assert_eq!(decoded.bytecodes.len(), 0);
        assert_eq!(decoded.profiles.len(), 0);
    }

    #[test]
    fn test_arch_detection() {
        let arch = current_arch();
        #[cfg(target_arch = "x86_64")]
        assert_eq!(arch, 0);
        #[cfg(target_arch = "aarch64")]
        assert_eq!(arch, 1);
    }
}
