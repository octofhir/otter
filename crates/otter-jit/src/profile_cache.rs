//! Persistent profile cache — save/load FeedbackVector data between runs.
//!
//! On repeated workloads, this allows the JIT to start with "warm" feedback
//! instead of re-collecting from scratch. The first JIT compilation can
//! immediately use profiles from the previous run.
//!
//! ## File format
//!
//! ```text
//! [OTTRPROF magic (8 bytes)]
//! [version: u32]
//! [entry_count: u32]
//! For each entry:
//!   [function_name: length-prefixed string]
//!   [source_hash: 16 bytes (truncated SHA for dedup)]
//!   [slot_count: u16]
//!   For each slot:
//!     [kind: u8 (0=arith, 1=cmp, 2=branch, 3=prop, 4=call)]
//!     [data: variable, depending on kind]
//! ```
//!
//! ## Invalidation
//!
//! Profiles are invalidated on:
//! - Engine version change
//! - Source file modification (hash mismatch)
//! - ABI version change
//!
//! Spec: Phase 8.2 of JIT_INCREMENTAL_PLAN.md

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::Path;

use otter_vm::feedback::{
    ArithmeticFeedback, ComparisonFeedback, FeedbackKind, FeedbackSlotId, FeedbackSlotLayout,
    FeedbackTableLayout, FeedbackVector,
};

const PROFILE_MAGIC: &[u8; 8] = b"OTTRPROF";
const PROFILE_VERSION: u32 = 1;

/// A serialized profile entry for one function.
#[derive(Debug, Clone)]
pub struct ProfileCacheEntry {
    /// Function name.
    pub function_name: String,
    /// Source hash (16 bytes, for invalidation).
    pub source_hash: [u8; 16],
    /// Serialized feedback slots.
    pub slots: Vec<SerializedSlot>,
}

/// A serialized feedback slot.
#[derive(Debug, Clone)]
pub struct SerializedSlot {
    pub kind: FeedbackKind,
    pub data: SlotData,
}

/// Serialized slot data.
#[derive(Debug, Clone)]
pub enum SlotData {
    Arithmetic(u8),      // ArithmeticFeedback as u8
    Comparison(u8),      // ComparisonFeedback as u8
    Branch { taken: u16, not_taken: u16 },
    Property(u8),        // 0=uninit, 1=mono, 2=poly, 3=mega (shapes not persisted)
    Call(u8),            // 0=uninit, 1=mono, 2=poly, 3=mega (targets not persisted)
}

/// Profile cache: stores profiles for multiple functions.
#[derive(Debug, Clone, Default)]
pub struct ProfileCache {
    entries: HashMap<String, ProfileCacheEntry>,
}

impl ProfileCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a profile entry.
    pub fn insert(&mut self, entry: ProfileCacheEntry) {
        self.entries.insert(entry.function_name.clone(), entry);
    }

    /// Look up a profile by function name.
    #[must_use]
    pub fn get(&self, function_name: &str) -> Option<&ProfileCacheEntry> {
        self.entries.get(function_name)
    }

    /// Number of cached profiles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialize a FeedbackVector into a cache entry.
    pub fn capture(
        function_name: &str,
        source_hash: [u8; 16],
        layout: &FeedbackTableLayout,
        vector: &FeedbackVector,
    ) -> ProfileCacheEntry {
        let mut slots = Vec::with_capacity(layout.len());
        for slot_layout in layout.slots() {
            let id = slot_layout.id();
            let kind = slot_layout.kind();
            let data = match kind {
                FeedbackKind::Arithmetic => {
                    let fb = vector.arithmetic(id).unwrap_or(ArithmeticFeedback::None);
                    SlotData::Arithmetic(fb as u8)
                }
                FeedbackKind::Comparison => {
                    let fb = vector
                        .get(id)
                        .and_then(|d| match d {
                            otter_vm::feedback::FeedbackSlotData::Comparison(c) => Some(*c),
                            _ => None,
                        })
                        .unwrap_or(ComparisonFeedback::None);
                    SlotData::Comparison(fb as u8)
                }
                FeedbackKind::Branch => {
                    let fb = vector.branch(id).unwrap_or_default();
                    SlotData::Branch { taken: fb.taken, not_taken: fb.not_taken }
                }
                FeedbackKind::Property => {
                    let state = vector.property(id).map_or(0u8, |p| {
                        use otter_vm::feedback::PropertyFeedback;
                        match p {
                            PropertyFeedback::Uninitialized => 0,
                            PropertyFeedback::Monomorphic(_) => 1,
                            PropertyFeedback::Polymorphic(_) => 2,
                            PropertyFeedback::Megamorphic => 3,
                        }
                    });
                    SlotData::Property(state)
                }
                FeedbackKind::Call => {
                    let state = vector.call(id).map_or(0u8, |c| {
                        use otter_vm::feedback::CallFeedback;
                        match c {
                            CallFeedback::Uninitialized => 0,
                            CallFeedback::Monomorphic(_) => 1,
                            CallFeedback::Polymorphic(_) => 2,
                            CallFeedback::Megamorphic => 3,
                        }
                    });
                    SlotData::Call(state)
                }
            };
            slots.push(SerializedSlot { kind, data });
        }
        ProfileCacheEntry {
            function_name: function_name.to_string(),
            source_hash,
            slots,
        }
    }

    /// Write the cache to a writer.
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(PROFILE_MAGIC)?;
        w.write_all(&PROFILE_VERSION.to_le_bytes())?;
        w.write_all(&(self.entries.len() as u32).to_le_bytes())?;

        for entry in self.entries.values() {
            write_str(w, &entry.function_name)?;
            w.write_all(&entry.source_hash)?;
            w.write_all(&(entry.slots.len() as u16).to_le_bytes())?;
            for slot in &entry.slots {
                w.write_all(&[kind_to_u8(slot.kind)])?;
                match &slot.data {
                    SlotData::Arithmetic(v) => w.write_all(&[*v])?,
                    SlotData::Comparison(v) => w.write_all(&[*v])?,
                    SlotData::Branch { taken, not_taken } => {
                        w.write_all(&taken.to_le_bytes())?;
                        w.write_all(&not_taken.to_le_bytes())?;
                    }
                    SlotData::Property(v) => w.write_all(&[*v])?,
                    SlotData::Call(v) => w.write_all(&[*v])?,
                }
            }
        }
        Ok(())
    }

    /// Read the cache from a reader.
    pub fn read_from(r: &mut impl Read) -> io::Result<Self> {
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if magic != *PROFILE_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
        }
        let mut buf4 = [0u8; 4];
        r.read_exact(&mut buf4)?;
        let version = u32::from_le_bytes(buf4);
        if version != PROFILE_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "version mismatch"));
        }
        r.read_exact(&mut buf4)?;
        let count = u32::from_le_bytes(buf4) as usize;

        let mut entries = HashMap::with_capacity(count);
        for _ in 0..count {
            let function_name = read_str(r)?;
            let mut source_hash = [0u8; 16];
            r.read_exact(&mut source_hash)?;
            let mut buf2 = [0u8; 2];
            r.read_exact(&mut buf2)?;
            let slot_count = u16::from_le_bytes(buf2) as usize;
            let mut slots = Vec::with_capacity(slot_count);
            for _ in 0..slot_count {
                let mut kb = [0u8; 1];
                r.read_exact(&mut kb)?;
                let kind = u8_to_kind(kb[0]);
                let data = match kind {
                    FeedbackKind::Arithmetic => { let mut b = [0u8; 1]; r.read_exact(&mut b)?; SlotData::Arithmetic(b[0]) }
                    FeedbackKind::Comparison => { let mut b = [0u8; 1]; r.read_exact(&mut b)?; SlotData::Comparison(b[0]) }
                    FeedbackKind::Branch => {
                        r.read_exact(&mut buf2)?; let taken = u16::from_le_bytes(buf2);
                        r.read_exact(&mut buf2)?; let not_taken = u16::from_le_bytes(buf2);
                        SlotData::Branch { taken, not_taken }
                    }
                    FeedbackKind::Property => { let mut b = [0u8; 1]; r.read_exact(&mut b)?; SlotData::Property(b[0]) }
                    FeedbackKind::Call => { let mut b = [0u8; 1]; r.read_exact(&mut b)?; SlotData::Call(b[0]) }
                };
                slots.push(SerializedSlot { kind, data });
            }
            entries.insert(function_name.clone(), ProfileCacheEntry { function_name, source_hash, slots });
        }
        Ok(Self { entries })
    }

    /// Save to a file path.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        self.write_to(&mut file)
    }

    /// Load from a file path. Returns empty cache on any error.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        std::fs::File::open(path)
            .ok()
            .and_then(|mut f| Self::read_from(&mut f).ok())
            .unwrap_or_default()
    }
}

fn kind_to_u8(k: FeedbackKind) -> u8 {
    match k {
        FeedbackKind::Arithmetic => 0,
        FeedbackKind::Comparison => 1,
        FeedbackKind::Branch => 2,
        FeedbackKind::Property => 3,
        FeedbackKind::Call => 4,
    }
}

fn u8_to_kind(v: u8) -> FeedbackKind {
    match v {
        0 => FeedbackKind::Arithmetic,
        1 => FeedbackKind::Comparison,
        2 => FeedbackKind::Branch,
        3 => FeedbackKind::Property,
        _ => FeedbackKind::Call,
    }
}

fn write_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    w.write_all(&(s.len() as u32).to_le_bytes())?;
    w.write_all(s.as_bytes())
}

fn read_str(r: &mut impl Read) -> io::Result<String> {
    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4)?;
    let len = u32::from_le_bytes(buf4) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_cache_roundtrip() {
        let layout = FeedbackTableLayout::new(vec![
            FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Arithmetic),
            FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Branch),
        ]);
        let mut fv = FeedbackVector::from_layout(&layout);
        fv.record_arithmetic(FeedbackSlotId(0), ArithmeticFeedback::Int32);
        fv.record_branch(FeedbackSlotId(1), true);
        fv.record_branch(FeedbackSlotId(1), true);
        fv.record_branch(FeedbackSlotId(1), false);

        let entry = ProfileCache::capture("test_fn", [0xAB; 16], &layout, &fv);
        assert_eq!(entry.slots.len(), 2);

        let mut cache = ProfileCache::new();
        cache.insert(entry);

        let mut buf = Vec::new();
        cache.write_to(&mut buf).unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let loaded = ProfileCache::read_from(&mut cursor).unwrap();
        assert_eq!(loaded.len(), 1);

        let loaded_entry = loaded.get("test_fn").unwrap();
        assert_eq!(loaded_entry.source_hash, [0xAB; 16]);
        assert_eq!(loaded_entry.slots.len(), 2);

        // Check arithmetic slot.
        assert!(matches!(loaded_entry.slots[0].data, SlotData::Arithmetic(1))); // Int32 = 1
        // Check branch slot.
        if let SlotData::Branch { taken, not_taken } = &loaded_entry.slots[1].data {
            assert_eq!(*taken, 2);
            assert_eq!(*not_taken, 1);
        } else {
            panic!("expected Branch slot");
        }
    }

    #[test]
    fn test_empty_cache_roundtrip() {
        let cache = ProfileCache::new();
        let mut buf = Vec::new();
        cache.write_to(&mut buf).unwrap();

        let mut cursor = io::Cursor::new(&buf);
        let loaded = ProfileCache::read_from(&mut cursor).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_invalid_magic_rejected() {
        let bad_data = b"BADMAGIC\x01\x00\x00\x00\x00\x00\x00\x00";
        let mut cursor = io::Cursor::new(&bad_data[..]);
        assert!(ProfileCache::read_from(&mut cursor).is_err());
    }
}
