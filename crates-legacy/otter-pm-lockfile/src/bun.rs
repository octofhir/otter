//! `bun.lock` adapter (Phase 2).
//!
//! bun 1.2+ defaults to the text format (`bun.lock`). The legacy
//! binary format (`bun.lockb`) is detected by [`crate::parse_lockfile_with_kind`]
//! and rejected with an actionable error directing the user to the
//! text variant.

use crate::{Error, LockfileGraph, LockfileKind};
use otter_pm_manifest::PackageJson;
use std::path::Path;

pub fn parse(_path: &Path) -> Result<LockfileGraph, Error> {
    Err(Error::Unsupported(LockfileKind::Bun))
}

pub fn write(_path: &Path, _graph: &LockfileGraph, _manifest: &PackageJson) -> Result<(), Error> {
    Err(Error::Unsupported(LockfileKind::Bun))
}
