//! `yarn.lock` adapter (Phase 2).
//!
//! `yarn.lock` comes in two incompatible dialects:
//! - **classic** (yarn v1): custom line-based text format.
//! - **berry** (yarn v2+): YAML with a `__metadata:` header and
//!   yarn-specific `resolution:` / `checksum:` fields.
//!
//! The dispatcher peeks the first ~200 bytes for `__metadata:` via
//! [`is_berry_path`] and upgrades [`crate::LockfileKind::Yarn`] →
//! [`crate::LockfileKind::YarnBerry`] before dispatching writes.

use crate::{Error, LockfileGraph, LockfileKind};
use otter_pm_manifest::PackageJson;
use std::io::Read;
use std::path::Path;

/// Return `true` if the yarn lockfile at `path` is yarn-berry format
/// (yarn v2+), `false` for yarn-classic (yarn v1). Errors bubble up as
/// `false` so mis-detection defaults to the older, more forgiving
/// parser — the classic parser's error message is clearer than a
/// berry YAML error would be on a classic file.
pub fn is_berry_path(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 256];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    std::str::from_utf8(&buf[..n])
        .map(|s| s.contains("__metadata:"))
        .unwrap_or(false)
}

pub fn parse(_path: &Path, _manifest: &PackageJson) -> Result<LockfileGraph, Error> {
    Err(Error::Unsupported(LockfileKind::Yarn))
}

pub fn write_classic(
    _path: &Path,
    _graph: &LockfileGraph,
    _manifest: &PackageJson,
) -> Result<(), Error> {
    Err(Error::Unsupported(LockfileKind::Yarn))
}

pub fn write_berry(
    _path: &Path,
    _graph: &LockfileGraph,
    _manifest: &PackageJson,
) -> Result<(), Error> {
    Err(Error::Unsupported(LockfileKind::YarnBerry))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_berry_peeks_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("yarn.lock");
        std::fs::write(&p, "# yarn lockfile v1\nfoo@^1:\n  version \"1.0.0\"\n").unwrap();
        assert!(!is_berry_path(&p));

        std::fs::write(
            &p,
            "__metadata:\n  version: 6\n\n\"foo@npm:^1\":\n  version: 1.0.0\n",
        )
        .unwrap();
        assert!(is_berry_path(&p));
    }
}
