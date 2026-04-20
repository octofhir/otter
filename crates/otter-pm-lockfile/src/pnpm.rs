//! `pnpm-lock.yaml` adapter (Phase 2).
//!
//! The pnpm v9 format is the richest of the external lockfiles we
//! plan to support — it carries per-importer direct deps with
//! specifiers, full peer-context keys, settings / overrides / catalogs
//! blocks, and time metadata. Implementing it well takes a real amount
//! of code (~1.7k LOC in the aube reference), so Phase 1 ships a stub
//! that tells callers explicitly when they hit pnpm territory.
//!
//! See `plans/partitioned-conjuring-church.md` § Phase 2 for the
//! completion criteria.

use crate::{Error, LockfileGraph, LockfileKind};
use otter_pm_manifest::PackageJson;
use std::path::Path;

pub fn parse(_path: &Path) -> Result<LockfileGraph, Error> {
    Err(Error::Unsupported(LockfileKind::Pnpm))
}

pub fn write(_path: &Path, _graph: &LockfileGraph, _manifest: &PackageJson) -> Result<(), Error> {
    Err(Error::Unsupported(LockfileKind::Pnpm))
}
