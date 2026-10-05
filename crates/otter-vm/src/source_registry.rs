//! Immutable source text and positions owned by one linked code chunk.
//!
//! # Contents
//! - [`ModuleSource`] owns admitted text and its immutable line-start index.
//! - [`SourceRegistry`] shares the exact source/index owner with linked code
//!   contexts and frozen code-space snapshots.
//!
//! # Invariants
//! - A URL is a label inside one chunk, never an interpreter-wide identity.
//! - Every retained text allocation has one [`SharedSource`] charge. Registry
//!   keys, records and line indexes have one separate metadata lease.
//! - Cloning a registry shares those immutable allocations and their charges;
//!   dropping the final owner releases both text and metadata.
//! - Preparation admits all metadata before publishing a registry. A refusal
//!   leaves the original source owners and code-space directory unchanged.
//! - Line and column are 1-based; columns count UTF-16 code units.
//! - An empty registry represents source-less bytecode without an allocation.
//!
//! # See also
//! - `crate::code_space` publishes the registry in its one chunk payload.
//! - `crate::stack_snapshot` resolves each frame through its actual chunk.
//! - `crate::object` stores eager diagnostic positions without pinning code.

use std::collections::BTreeMap;
use std::sync::Arc;

use otter_resource::{ResourceAccount, ResourceClass, ResourceError, ResourceLease, SharedSource};

/// One module's source text with a precomputed line-start index.
#[derive(Debug)]
pub struct ModuleSource {
    text: SharedSource,
    /// Byte offset of the first character of each line. `line_starts[0]`
    /// is always `0`; entry `n` is the byte offset just past the `n`th
    /// `\n`. Sorted ascending, so a byte offset maps to a line by
    /// `partition_point`.
    line_starts: Box<[u32]>,
}

impl ModuleSource {
    /// Build a source entry, scanning once for line starts.
    fn new(text: SharedSource, lease: &mut ResourceLease) -> Result<Self, ResourceError> {
        let capacity = 1 + text.bytes().filter(|&byte| byte == b'\n').count();
        let mut line_starts = crate::executable::allocation::try_vec(capacity, lease)?;
        line_starts.push(0);
        for (idx, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push((idx + 1) as u32);
            }
        }
        Ok(Self {
            text,
            line_starts: line_starts.into_boxed_slice(),
        })
    }

    /// Resolve a 0-based byte offset into a 1-based `(line, column)`
    /// position. Column is measured in UTF-16 code units from the start
    /// of the line. Offsets past the end clamp to the final line; an offset
    /// inside a UTF-8 code point clamps to its preceding character boundary.
    pub fn line_col(&self, byte_offset: u32) -> (u32, u32) {
        let mut clamped = (byte_offset as usize).min(self.text.len());
        while !self.text.is_char_boundary(clamped) {
            clamped -= 1;
        }
        let clamped = clamped as u32;
        // `partition_point` returns the count of line starts `<= clamped`;
        // since `line_starts[0] == 0`, that count is the 1-based line.
        let line = self.line_starts.partition_point(|&s| s <= clamped);
        let line = line.max(1);
        let line_start = self.line_starts[line - 1] as usize;
        let slice = &self.text[line_start..clamped as usize];
        let col_units = slice.chars().map(|c| c.len_utf16()).sum::<usize>();
        (line as u32, (col_units as u32) + 1)
    }

    /// The verbatim source text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Share one exact source line with the original text's physical lease.
    /// The line remains valid after its defining code has been evicted.
    pub fn line_source(&self, line_number: u32) -> Option<SharedSource> {
        let line = self.line_text(line_number)?;
        let start = *self.line_starts.get(line_number.checked_sub(1)? as usize)? as usize;
        self.text.slice(start..start + line.len())
    }

    /// Return one 1-based source line without its trailing line break.
    pub fn line_text(&self, line_number: u32) -> Option<&str> {
        if line_number == 0 {
            return None;
        }
        let idx = (line_number - 1) as usize;
        let start = *self.line_starts.get(idx)? as usize;
        let end = self
            .line_starts
            .get(idx + 1)
            .map(|n| *n as usize)
            .unwrap_or(self.text.len());
        Some(self.text[start..end].trim_end_matches(['\r', '\n']))
    }
}

/// Immutable URL entries for one linked chunk, shared with its snapshots.
///
/// This wrapper contains no mutable registry or resource-policy state. Every
/// clone refers to the same exact source allocations and retained-byte lease.
#[derive(Debug, Clone, Default)]
pub struct SourceRegistry {
    inner: Option<Arc<SourceRegistryInner>>,
}

#[derive(Debug)]
struct SourceRegistryInner {
    // Field order releases physical entries before their accounting lease.
    entries: Box<[(Box<str>, ModuleSource)]>,
    // Nonempty metadata always reserves SourceModuleBytes. Taking its sole
    // lease from the existing atomic reservation owner avoids a second ledger.
    _metadata_lease: Option<ResourceLease>,
}

impl SourceRegistry {
    /// Prepare complete immutable entries against the shared resource ledger.
    ///
    /// Already-admitted source handles keep their original text charge. This
    /// operation charges only the URL/index metadata it will retain. Empty
    /// input remains an allocation-free, source-less registry.
    ///
    /// # Errors
    /// Returns the actual ledger refusal before retaining metadata or
    /// publishing code. The canonical reservation owner detects integer
    /// overflow while aggregating exact retained allocations.
    pub fn new(
        entries: BTreeMap<String, SharedSource>,
        account: &ResourceAccount,
    ) -> Result<Self, ResourceError> {
        if entries.is_empty() {
            return Ok(Self::default());
        }
        let class = ResourceClass::SourceModuleBytes;
        let amounts = std::iter::once((class, std::mem::size_of::<SourceRegistryInner>() as u64))
            .chain(entries.iter().flat_map(|(url, text)| {
                std::iter::once((
                    class,
                    std::mem::size_of::<(Box<str>, ModuleSource)>() as u64,
                ))
                .chain(std::iter::once((class, url.len() as u64)))
                .chain(std::iter::once((class, std::mem::size_of::<u32>() as u64)))
                .chain(
                    text.bytes()
                        .filter(|&byte| byte == b'\n')
                        .map(move |_| (class, std::mem::size_of::<u32>() as u64)),
                )
            }));
        let mut leases = account.reserve_exact_many(amounts)?;
        let mut metadata_lease = leases.take(class);
        let lease = metadata_lease
            .as_mut()
            .expect("nonempty source metadata has its admitted lease");
        let requested = lease.amount();
        let capacity = entries.len();
        let mut prepared = crate::executable::allocation::try_vec(capacity, lease)?;
        for (url, text) in entries {
            prepared.push((url.into_boxed_str(), ModuleSource::new(text, lease)?));
        }
        let entries = prepared.into_boxed_slice();
        lease.resize(requested)?;
        Ok(Self {
            inner: Some(Arc::new(SourceRegistryInner {
                entries,
                _metadata_lease: metadata_lease,
            })),
        })
    }

    /// Resolve a URL inside this exact linked chunk's immutable sources.
    #[must_use]
    pub fn get(&self, module_url: &str) -> Option<&ModuleSource> {
        let entries = &self.inner.as_ref()?.entries;
        let index = entries
            .binary_search_by(|(url, _)| url.as_ref().cmp(module_url))
            .ok()?;
        Some(&entries[index].1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module_source(text: SharedSource) -> Result<ModuleSource, ResourceError> {
        let capacity = 1 + text.bytes().filter(|&byte| byte == b'\n').count();
        let mut lease = ResourceAccount::default().reserve_exact(
            ResourceClass::SourceModuleBytes,
            (capacity as u64).saturating_mul(std::mem::size_of::<u32>() as u64),
        )?;
        ModuleSource::new(text, &mut lease)
    }

    fn source(text: &str) -> SharedSource {
        SharedSource::admit(&ResourceAccount::default(), text.to_string()).unwrap()
    }

    #[test]
    fn line_col_basic() {
        let src = module_source(source("ab\ncde\nf")).unwrap();
        // offset 0 -> line 1 col 1
        assert_eq!(src.line_col(0), (1, 1));
        // offset 1 -> line 1 col 2
        assert_eq!(src.line_col(1), (1, 2));
        // offset 3 -> first char of line 2 ('c')
        assert_eq!(src.line_col(3), (2, 1));
        // offset 5 -> 'e' on line 2, col 3
        assert_eq!(src.line_col(5), (2, 3));
        // offset 7 -> 'f' on line 3
        assert_eq!(src.line_col(7), (3, 1));
    }

    #[test]
    fn line_col_utf16_columns() {
        // 'é' is 2 bytes UTF-8, 1 UTF-16 unit. Column after it is 2.
        let src = module_source(source("é x")).unwrap();
        // byte offset 2 is the space (after the 2-byte 'é')
        assert_eq!(src.line_col(2), (1, 2));
    }

    #[test]
    fn utf8_interiors_clamp_without_breaking_utf16_columns_or_shared_lines() {
        let account = ResourceAccount::default();
        let text = SharedSource::admit(&account, "é𝄞 x\nlast".to_owned()).unwrap();
        let sources =
            SourceRegistry::new(BTreeMap::from([("utf8.js".to_owned(), text)]), &account).unwrap();
        let source = sources.get("utf8.js").unwrap();
        assert_eq!(source.line_col(1), (1, 1));
        for offset in 3..6 {
            assert_eq!(source.line_col(offset), (1, 2));
        }
        assert_eq!(source.line_col(6), (1, 4));
        assert_eq!(source.line_col(9), (2, 1));
        let line = source.line_source(1).unwrap();
        assert_eq!(line.as_ref(), "é𝄞 x");
        assert_eq!(line.as_ref().as_ptr(), source.text().as_ptr());
        assert!(source.line_source(0).is_none());
        assert!(source.line_source(3).is_none());
    }

    #[test]
    fn clamps_past_end() {
        let src = module_source(source("abc")).unwrap();
        assert_eq!(src.line_col(999), (1, 4));
    }

    #[test]
    fn same_url_registries_and_snapshot_clones_retain_exact_independent_charges() {
        fn current(account: &ResourceAccount) -> u64 {
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current()
        }
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SourceRegistry>();
        let account = ResourceAccount::default();
        let a_text = SharedSource::admit(&account, "original\nsource A".to_owned())
            .expect("first source bytes");
        let a = SourceRegistry::new(BTreeMap::from([("same.js".to_owned(), a_text)]), &account)
            .expect("first metadata");
        let a_charge = current(&account);
        let snapshot = a.clone();
        assert_eq!(current(&account), a_charge, "clone shares one charge");
        assert!(std::ptr::eq(
            a.get("same.js").unwrap(),
            snapshot.get("same.js").unwrap()
        ));

        let b_text = SharedSource::admit(
            &account,
            "different source B\nwith new geometry\n".to_owned(),
        )
        .expect("second source bytes");
        let b = SourceRegistry::new(BTreeMap::from([("same.js".to_owned(), b_text)]), &account)
            .expect("second metadata");
        let both = current(&account);
        assert!(both > a_charge, "independent second allocation is admitted");
        assert_eq!(a.get("same.js").unwrap().line_text(2), Some("source A"));
        assert_eq!(
            b.get("same.js").unwrap().line_text(2),
            Some("with new geometry")
        );
        drop(a);
        assert_eq!(
            current(&account),
            both,
            "snapshot retains the first exact allocation"
        );
        drop(snapshot);
        assert_eq!(
            current(&account),
            both - a_charge,
            "only first text/index owner released"
        );
        drop(b);
        assert_eq!(
            current(&account),
            0,
            "final source/index charge released once"
        );
    }

    #[test]
    fn metadata_refusal_preserves_actual_resource_cause_and_original_source_owner() {
        let text = "source still retained";
        let limit = text.len() as u64;
        let account = ResourceAccount::new(
            otter_resource::ResourceLimits::builder()
                .limit(ResourceClass::SourceModuleBytes, limit)
                .build(),
        );
        let retained =
            SharedSource::admit(&account, text.to_owned()).expect("exact text admission");
        let before = account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .clone();
        let error = SourceRegistry::new(
            BTreeMap::from([("same.js".to_owned(), retained.clone())]),
            &account,
        )
        .expect_err("no remaining metadata headroom");
        assert!(matches!(error, ResourceError::Exhausted {
            class: ResourceClass::SourceModuleBytes, requested, in_use, limit: actual_limit,
        } if requested > 0 && in_use == limit && actual_limit == limit));
        let after = account
            .snapshot()
            .get(ResourceClass::SourceModuleBytes)
            .clone();
        assert_eq!(after.current(), before.current());
        assert_eq!(after.peak(), before.peak());
        assert_eq!(after.rejections(), before.rejections() + 1);
        assert_eq!(retained.as_ref(), text);
        drop(retained);
        assert_eq!(
            account
                .snapshot()
                .get(ResourceClass::SourceModuleBytes)
                .current(),
            0
        );
    }

    #[test]
    fn registry_roundtrip() {
        let reg = SourceRegistry::new(
            BTreeMap::from([("file:///a.js".to_owned(), source("x\ny"))]),
            &ResourceAccount::default(),
        )
        .expect("source metadata admission");
        assert_eq!(
            reg.get("file:///a.js").map(|source| source.line_col(2)),
            Some((2, 1))
        );
        assert!(reg.get("file:///missing.js").is_none());
    }
}
