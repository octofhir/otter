//! `package-lock.json` + `npm-shrinkwrap.json` adapter (Phase 2).
//!
//! Both files share the same schema; the two variants in
//! [`crate::LockfileKind`] exist so the writer can preserve whichever
//! filename was on disk.

use crate::{Error, LockfileGraph, LockfileKind};
use otter_pm_manifest::PackageJson;
use std::path::Path;

pub fn parse(_path: &Path) -> Result<LockfileGraph, Error> {
    Err(Error::Unsupported(LockfileKind::Npm))
}

pub fn write(_path: &Path, _graph: &LockfileGraph, _manifest: &PackageJson) -> Result<(), Error> {
    Err(Error::Unsupported(LockfileKind::Npm))
}
