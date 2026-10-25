//! Isolated `node_modules` layout.
//!
//! Every installed package version lives in exactly one real directory,
//! `node_modules/.otter/<entry>/node_modules/<name>`, beside links to the
//! packages it depends on: Node's upward module lookup from inside a package
//! finds its own dependencies first, at the versions the lockfile resolved,
//! however many versions of a name the graph holds. The project's direct
//! dependencies are links at `node_modules/<name>`.
//!
//! A package that requires something it never declared still finds it:
//! `node_modules/.otter/node_modules` holds one link per installed name, which
//! every package directory reaches before the project's own `node_modules`.
//!
//! # Contents
//! - [`package_dir`] — where one package version is materialized, inside its
//!   [`store_entry_dir`].
//! - [`link_project`] — link every package, workspace and the project to the
//!   packages the lockfile records their edges resolved to.
//!
//! # Invariants
//! - A package id names one directory: the same id always lands in the same
//!   place, and two ids never share one.
//! - Links are relative, so a project directory can move.
//! - Linking is idempotent: a link already pointing at its target is kept;
//!   anything else at a link's path is replaced.
//! - A link is created only to a package directory that exists; an optional
//!   dependency that was skipped leaves no dangling link.
//!
//! # See also
//! - [`crate::install`] for materializing package directories.
//! - [`crate::installed_graph`] for reading the layout back.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use otter_pm_lockfile::{Lockfile, ResolvedSourceKind};

use crate::{PackageGraph, PackageId, PackageManagerError, cache_key, package_name_path};

/// Directory under `node_modules` holding every installed package version.
pub(crate) const VIRTUAL_STORE: &str = ".otter";

/// The virtual-store entry naming one package id: `name@version` for a
/// registry version (`/` of a scope as `+`), the escaped id otherwise.
fn store_entry(id: &str) -> String {
    let entry = id.replacen("@npm:", "@", 1).replace('/', "+");
    if entry
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '+' | '.' | '-' | '_'))
    {
        entry
    } else {
        cache_key(id)
    }
}

/// The virtual-store directory holding package `id` and its dependency
/// links.
#[must_use]
pub(crate) fn store_entry_dir(project_root: &Path, id: &str) -> PathBuf {
    project_root
        .join("node_modules")
        .join(VIRTUAL_STORE)
        .join(store_entry(id))
}

/// The `node_modules` directory a stored package shares with the links to
/// its dependencies.
fn store_modules_dir(project_root: &Path, id: &str) -> PathBuf {
    store_entry_dir(project_root, id).join("node_modules")
}

/// The real directory package `id`, installed as `name`, is materialized in.
#[must_use]
pub(crate) fn package_dir(project_root: &Path, id: &str, name: &str) -> PathBuf {
    store_modules_dir(project_root, id).join(package_name_path(name))
}

/// `true` for a package materialized from a tarball into the virtual store.
pub(crate) fn is_stored(lockfile: &Lockfile, id: &str) -> bool {
    lockfile.packages.get(id).is_some_and(|package| {
        package.resolved.as_ref().is_some_and(|source| {
            matches!(
                source.kind,
                ResolvedSourceKind::Registry | ResolvedSourceKind::Tarball
            ) && crate::is_tarball_reference(&source.reference)
        })
    })
}

/// Link every edge `lockfile` records: a stored package's dependencies
/// beside it, a workspace's (and the project's) in its own `node_modules`,
/// and one link per installed name into the shared fallback directory.
/// `graph` places the packages that are not stored.
///
/// # Errors
/// A filesystem failure creating or replacing a link.
pub(crate) async fn link_project(
    project_root: &Path,
    graph: &PackageGraph,
    lockfile: &Lockfile,
) -> Result<(), PackageManagerError> {
    let root_of = |id: &str| -> Option<PathBuf> {
        if is_stored(lockfile, id) {
            let package = lockfile.packages.get(id)?;
            Some(package_dir(project_root, id, &package.name))
        } else {
            graph
                .packages
                .get(&PackageId::new(id))
                .map(|package| package.root.clone())
        }
    };
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (from, package) in &lockfile.packages {
        let stored = is_stored(lockfile, from);
        let modules = if stored {
            store_modules_dir(project_root, from)
        } else if let Some(root) = root_of(from) {
            root.join("node_modules")
        } else {
            continue;
        };
        for (name, target) in &package.dependencies {
            if stored && *name == package.name {
                continue;
            }
            if let Some(target_root) = root_of(target) {
                links.push((modules.join(package_name_path(name)), target_root));
            }
        }
    }
    // One version per name for undeclared requires: the newest installed.
    let mut fallback: BTreeMap<&str, (Option<semver::Version>, &str)> = BTreeMap::new();
    for (id, package) in &lockfile.packages {
        if !is_stored(lockfile, id) {
            continue;
        }
        let version = semver::Version::parse(&package.version).ok();
        let newer = fallback
            .get(package.name.as_str())
            .is_none_or(|(held, _)| version > *held);
        if newer {
            fallback.insert(&package.name, (version, id));
        }
    }
    let fallback_root = project_root
        .join("node_modules")
        .join(VIRTUAL_STORE)
        .join("node_modules");
    for (name, (_, id)) in fallback {
        links.push((
            fallback_root.join(package_name_path(name)),
            package_dir(project_root, id, name),
        ));
    }
    tokio::task::spawn_blocking(move || {
        for (link, target) in &links {
            if target.exists() {
                ensure_link(link, target)?;
            }
        }
        Ok(())
    })
    .await
    .map_err(|err| PackageManagerError::Io {
        path: project_root.to_path_buf(),
        message: err.to_string(),
    })?
}

fn io_error(path: &Path, err: &std::io::Error) -> PackageManagerError {
    PackageManagerError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

/// Make `link` a relative link to directory `target`.
fn ensure_link(link: &Path, target: &Path) -> Result<(), PackageManagerError> {
    let parent = link.parent().unwrap_or(link);
    let relative = relative_path(parent, target);
    match std::fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if std::fs::read_link(link).is_ok_and(|current| current == relative) {
                return Ok(());
            }
            std::fs::remove_file(link).map_err(|err| io_error(link, &err))?;
        }
        Ok(metadata) if metadata.is_dir() => {
            std::fs::remove_dir_all(link).map_err(|err| io_error(link, &err))?;
        }
        Ok(_) => std::fs::remove_file(link).map_err(|err| io_error(link, &err))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(io_error(link, &err)),
    }
    std::fs::create_dir_all(parent).map_err(|err| io_error(parent, &err))?;
    symlink_dir(&relative, link).map_err(|err| io_error(link, &err))
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    // A junction needs no privilege, and only takes an absolute target.
    let absolute = link.parent().unwrap_or(link).join(target);
    std::os::windows::fs::symlink_dir(&absolute, link)
}

/// `target` as seen from directory `from`; both are absolute paths.
fn relative_path(from: &Path, target: &Path) -> PathBuf {
    let from: Vec<Component<'_>> = from.components().collect();
    let target: Vec<Component<'_>> = target.components().collect();
    let shared = from.iter().zip(&target).take_while(|(a, b)| a == b).count();
    let mut relative = PathBuf::new();
    for _ in shared..from.len() {
        relative.push("..");
    }
    for component in &target[shared..] {
        relative.push(component.as_os_str());
    }
    relative
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registry_version_gets_a_readable_entry() {
        assert_eq!(store_entry("ms@npm:2.0.0"), "ms@2.0.0");
        assert_eq!(store_entry("@types/node@npm:22.1.0"), "@types+node@22.1.0");
        assert_eq!(
            store_entry("tool@tarball:https://example.test/tool.tgz"),
            cache_key("tool@tarball:https://example.test/tool.tgz")
        );
    }

    #[test]
    fn a_relative_link_climbs_to_the_shared_ancestor() {
        assert_eq!(
            relative_path(
                Path::new("/p/node_modules/.otter/a@1.0.0/node_modules"),
                Path::new("/p/node_modules/.otter/b@2.0.0/node_modules/b"),
            ),
            PathBuf::from("../../b@2.0.0/node_modules/b")
        );
        assert_eq!(
            relative_path(
                Path::new("/p/node_modules/@scope"),
                Path::new("/p/node_modules/.otter/@scope+x@1.0.0/node_modules/@scope/x"),
            ),
            PathBuf::from("../.otter/@scope+x@1.0.0/node_modules/@scope/x")
        );
    }
}
