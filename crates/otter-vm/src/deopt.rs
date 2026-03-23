//! Deoptimization metadata for the new VM.

use crate::bytecode::ProgramCounter;

/// Stable identifier of a deoptimization site in compiled code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeoptId(pub u32);

/// Static metadata for a single deoptimization site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeoptSite {
    id: DeoptId,
    pc: ProgramCounter,
}

impl DeoptSite {
    /// Creates a deoptimization site.
    #[must_use]
    pub const fn new(id: DeoptId, pc: ProgramCounter) -> Self {
        Self { id, pc }
    }

    /// Returns the site identifier.
    #[must_use]
    pub const fn id(self) -> DeoptId {
        self.id
    }

    /// Returns the program counter that owns the deopt site.
    #[must_use]
    pub const fn pc(self) -> ProgramCounter {
        self.pc
    }
}

/// Why compiled code handed execution back to the interpreter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeoptReason {
    /// A speculative guard in compiled code failed.
    GuardFailure,
    /// Compiled code hit a path that still requires interpreter execution.
    UnsupportedPath,
    /// Compiled code needs the interpreter to materialize state explicitly.
    Materialization,
}

/// Placeholder handoff record shared by future JIT deopt sites and interpreter resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeoptHandoff {
    site: DeoptSite,
    resume_pc: ProgramCounter,
    reason: DeoptReason,
}

impl DeoptHandoff {
    /// Creates a deopt handoff placeholder from one static site.
    #[must_use]
    pub const fn new(site: DeoptSite, resume_pc: ProgramCounter, reason: DeoptReason) -> Self {
        Self {
            site,
            resume_pc,
            reason,
        }
    }

    /// Creates a deopt handoff that resumes at the owning bytecode PC.
    #[must_use]
    pub const fn at_site(site: DeoptSite, reason: DeoptReason) -> Self {
        Self::new(site, site.pc(), reason)
    }

    /// Returns the static deopt site.
    #[must_use]
    pub const fn site(self) -> DeoptSite {
        self.site
    }

    /// Returns the interpreter resume PC.
    #[must_use]
    pub const fn resume_pc(self) -> ProgramCounter {
        self.resume_pc
    }

    /// Returns the deopt reason.
    #[must_use]
    pub const fn reason(self) -> DeoptReason {
        self.reason
    }
}

/// Immutable table of deoptimization sites for a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeoptTable {
    sites: Box<[DeoptSite]>,
}

impl DeoptTable {
    /// Creates a deoptimization table from owned site metadata.
    #[must_use]
    pub fn new(sites: Vec<DeoptSite>) -> Self {
        Self {
            sites: sites.into_boxed_slice(),
        }
    }

    /// Creates an empty deoptimization table.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Returns the number of sites in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sites.len()
    }

    /// Returns `true` when the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    /// Returns the immutable site slice.
    #[must_use]
    pub fn sites(&self) -> &[DeoptSite] {
        &self.sites
    }

    /// Returns the deopt site for a given bytecode PC.
    #[must_use]
    pub fn site_for_pc(&self, pc: ProgramCounter) -> Option<DeoptSite> {
        self.sites.iter().copied().find(|site| site.pc() == pc)
    }
}

impl Default for DeoptTable {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{DeoptHandoff, DeoptId, DeoptReason, DeoptSite, DeoptTable};

    #[test]
    fn deopt_table_resolves_site_by_pc() {
        let site = DeoptSite::new(DeoptId(4), 9);
        let table = DeoptTable::new(vec![site]);

        assert_eq!(table.site_for_pc(9), Some(site));
        assert_eq!(table.site_for_pc(10), None);
    }

    #[test]
    fn deopt_handoff_defaults_resume_to_site_pc() {
        let site = DeoptSite::new(DeoptId(7), 13);
        let handoff = DeoptHandoff::at_site(site, DeoptReason::GuardFailure);

        assert_eq!(handoff.site(), site);
        assert_eq!(handoff.resume_pc(), 13);
        assert_eq!(handoff.reason(), DeoptReason::GuardFailure);
    }
}
