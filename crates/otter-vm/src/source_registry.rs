//! Per-interpreter registry of module source text, used to resolve a
//! bytecode instruction's byte span back to a human `(line, column)`
//! position for `Error.prototype.stack` and `util.getCallSites`.
//!
//! # Contents
//! - [`ModuleSource`] — one module's verbatim source plus a precomputed
//!   line-start index.
//! - [`SourceRegistry`] — `module_url → ModuleSource` map owned by the
//!   [`crate::Interpreter`], with position and source-line lookup helpers.
//!
//! # Invariants
//! - Line and column numbers are **1-based**, matching V8 / Node call
//!   sites (`CallSite.getLineNumber` / `getColumnNumber`).
//! - Columns are counted in UTF-16 code units (the unit ECMAScript and
//!   V8 expose), so multi-byte source still reports V8-compatible
//!   columns.
//! - `line_starts` is computed once at registration; the VM never holds
//!   a mutable borrow of a registered source, so no interior mutability
//!   is needed.
//! - Every retained source carries its exact `SourceModuleBytes` charge:
//!   loader-produced text arrives as an already-admitted
//!   [`SharedSource`], and VM-synthesized text (eval/CommonJS wrappers)
//!   is admitted against the registry's account before retention.
//!
//! # See also
//! - [`crate::stack_snapshot::snapshot_frames`] — produces the
//!   `(function, module, span)` frames this registry maps to positions.

use otter_resource::{ResourceAccount, SharedSource, SharedSourceError};
use rustc_hash::FxHashMap;

/// One module's source text with a precomputed line-start index.
#[derive(Debug, Clone)]
pub struct ModuleSource {
    text: SharedSource,
    /// Byte offset of the first character of each line. `line_starts[0]`
    /// is always `0`; entry `n` is the byte offset just past the `n`th
    /// `\n`. Sorted ascending, so a byte offset maps to a line by
    /// `partition_point`.
    line_starts: Vec<u32>,
}

impl ModuleSource {
    /// Build a source entry, scanning once for line starts.
    pub fn new(text: SharedSource) -> Self {
        let mut line_starts = Vec::with_capacity(64);
        line_starts.push(0);
        for (idx, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                // Next line starts at the byte after the newline.
                line_starts.push((idx + 1) as u32);
            }
        }
        Self { text, line_starts }
    }

    /// Resolve a 0-based byte offset into a 1-based `(line, column)`
    /// position. Column is measured in UTF-16 code units from the start
    /// of the line. Offsets past the end clamp to the final line.
    pub fn line_col(&self, byte_offset: u32) -> (u32, u32) {
        let clamped = byte_offset.min(self.text.len() as u32);
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

/// `module_url → ModuleSource` registry owned by the interpreter.
#[derive(Debug, Default)]
pub struct SourceRegistry {
    /// Ledger charged for VM-synthesized retained sources. The runtime
    /// installs its shared account at construction; the default is a
    /// private unlimited account, matching `ResourceLimits::unlimited`.
    account: ResourceAccount,
    sources: FxHashMap<String, ModuleSource>,
}

impl SourceRegistry {
    /// Install the ledger charged for VM-synthesized retained sources.
    pub fn set_account(&mut self, account: ResourceAccount) {
        self.account = account;
    }

    /// The ledger charged for VM-synthesized retained sources.
    #[must_use]
    pub fn account(&self) -> &ResourceAccount {
        &self.account
    }

    /// Register (or replace) a module's already-admitted source text.
    /// Idempotent re-loads simply rebuild the line index.
    pub fn register(&mut self, module_url: impl Into<String>, text: SharedSource) {
        self.sources
            .insert(module_url.into(), ModuleSource::new(text));
    }

    /// Admit a VM-synthesized source against the registry's account and
    /// register it.
    ///
    /// # Errors
    /// Returns the admission failure without retaining anything.
    pub fn register_owned(
        &mut self,
        module_url: impl Into<String>,
        text: String,
    ) -> Result<(), SharedSourceError> {
        let text = SharedSource::admit(&self.account, text)?;
        self.register(module_url, text);
        Ok(())
    }

    /// Look up a registered module's source.
    pub fn get(&self, module_url: &str) -> Option<&ModuleSource> {
        self.sources.get(module_url)
    }

    /// Resolve a `(module_url, byte_offset)` pair to a 1-based
    /// `(line, column)` position, when the module's source is known.
    pub fn line_col(&self, module_url: &str, byte_offset: u32) -> Option<(u32, u32)> {
        self.sources
            .get(module_url)
            .map(|s| s.line_col(byte_offset))
    }

    /// Look up the registered text for one 1-based source line.
    pub fn line_text(&self, module_url: &str, line_number: u32) -> Option<&str> {
        self.sources
            .get(module_url)
            .and_then(|s| s.line_text(line_number))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(text: &str) -> SharedSource {
        SharedSource::admit(&ResourceAccount::default(), text.to_string()).unwrap()
    }

    #[test]
    fn line_col_basic() {
        let src = ModuleSource::new(source("ab\ncde\nf"));
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
        let src = ModuleSource::new(source("é x"));
        // byte offset 2 is the space (after the 2-byte 'é')
        assert_eq!(src.line_col(2), (1, 2));
    }

    #[test]
    fn clamps_past_end() {
        let src = ModuleSource::new(source("abc"));
        assert_eq!(src.line_col(999), (1, 4));
    }

    #[test]
    fn registry_roundtrip() {
        let mut reg = SourceRegistry::default();
        reg.register("file:///a.js", source("x\ny"));
        assert_eq!(reg.line_col("file:///a.js", 2), Some((2, 1)));
        assert_eq!(reg.line_col("file:///missing.js", 0), None);
    }
}
