//! One-off package binaries.
//!
//! `otterx <package>` runs a package's binary without adding the package to
//! the project. A binary the project already installed is used as-is — the
//! version a project pinned is the version its tooling should run — and only a
//! package that is genuinely absent is fetched into a throwaway environment.
//!
//! # Contents
//! - [`DlxArgs`] — the command's arguments.
//! - [`resolve_dlx_target`] — find the binary to run, fetching if needed.
//! - [`DlxTarget`] — where the binary came from and where it lives.
//!
//! # Invariants
//! - Local before remote. An installed binary wins over a fetched one, and no
//!   network request happens when the project already has the tool.
//! - A fetched environment is keyed by the exact specifier, so running the same
//!   tool twice reuses it and running a different version does not collide.
//! - Fetched environments live under the user cache beside the content store,
//!   so the packages they install are shared like every other install.
//! - Fetching goes through the same install path as a project install, so the
//!   same security gates apply to a one-off tool.

use std::path::{Path, PathBuf};

use clap::Args;
use otter_pm_manifest::PACKAGE_JSON;

use crate::OtterError;

/// Arguments for a one-off binary run.
#[derive(Debug, Args)]
pub(crate) struct DlxArgs {
    /// Package specifier, for example `esbuild` or `typescript@5.9`.
    pub(crate) package: String,
    /// Binary name, when it differs from the package name.
    #[arg(long)]
    pub(crate) bin: Option<String>,
    /// Forwarded binary arguments.
    #[arg(trailing_var_arg = true)]
    pub(crate) args: Vec<String>,
}

/// Where a one-off binary came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DlxTarget {
    /// Project root whose installed tree provides the binary.
    pub(crate) root: PathBuf,
    /// Binary name to resolve inside that tree.
    pub(crate) bin: String,
    /// `true` when the binary was already installed for the project.
    pub(crate) local: bool,
}

/// Find the tree providing the binary `args` names, fetching the package when
/// the project does not already provide it.
pub(crate) async fn resolve_dlx_target(
    project_root: &Path,
    args: &DlxArgs,
) -> Result<DlxTarget, OtterError> {
    let (package, range) = split_package_spec(&args.package);
    let bin = args.bin.clone().unwrap_or_else(|| binary_name(&package));

    if let Some(root) = find_installing_root(project_root, &bin).await {
        return Ok(DlxTarget {
            root,
            bin,
            local: true,
        });
    }

    let environment = dlx_environment_root(&args.package);
    install_dlx_environment(&environment, &package, &range).await?;
    if !tokio::fs::try_exists(environment.join("node_modules").join(".bin").join(&bin))
        .await
        .unwrap_or(false)
    {
        return Err(crate::pm_config_error(format!(
            "`{package}` does not provide a binary named `{bin}`"
        )));
    }
    Ok(DlxTarget {
        root: environment,
        bin,
        local: false,
    })
}

/// The file a tree's `.bin` entry points at, for a tree whose package graph
/// cannot be resolved.
///
/// A launcher's own name carries no source kind, but our launchers are links,
/// and following one lands on the package file the manifest would have named.
pub(crate) async fn launcher_source_path(target: &DlxTarget) -> Option<PathBuf> {
    let launcher = target
        .root
        .join("node_modules")
        .join(".bin")
        .join(&target.bin);
    let resolved = tokio::fs::canonicalize(launcher).await.ok()?;
    otter_syntax::detect_source_kind(&resolved)
        .is_some()
        .then_some(resolved)
}

/// Walk up from `project_root` for the nearest tree that installed `bin`.
///
/// Walking upward matches how a module resolves: a tool installed at a
/// workspace root serves the member directories under it.
async fn find_installing_root(project_root: &Path, bin: &str) -> Option<PathBuf> {
    let mut current = Some(project_root);
    while let Some(directory) = current {
        let candidate = directory.join("node_modules").join(".bin").join(bin);
        if tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            return Some(directory.to_path_buf());
        }
        current = directory.parent();
    }
    None
}

async fn install_dlx_environment(
    environment: &Path,
    package: &str,
    range: &str,
) -> Result<(), OtterError> {
    tokio::fs::create_dir_all(environment)
        .await
        .map_err(|err| crate::pm_config_error(err.to_string()))?;
    let manifest = format!(
        "{{\"name\":\"otter-dlx\",\"private\":true,\"dependencies\":{{{}:{}}}}}",
        serde_json::to_string(package).unwrap_or_else(|_| "\"\"".to_string()),
        serde_json::to_string(range).unwrap_or_else(|_| "\"*\"".to_string()),
    );
    tokio::fs::write(environment.join(PACKAGE_JSON), manifest)
        .await
        .map_err(|err| crate::pm_config_error(err.to_string()))?;

    let cache_root = otter_pm::user_cache_root();
    otter_pm::install_local_project(
        environment,
        &otter_pm::FsRegistryMetadataCache::new(cache_root.join("registry-metadata")),
        &otter_pm::HttpRegistryMetadataClient::new(),
        &otter_pm::FsPackageStore::new(cache_root),
        &otter_pm::HttpTarballClient::new(),
        &otter_pm::HttpAdvisoryClient::new(),
    )
    .await
    .map(|_| ())
    .map_err(crate::map_pm_error)
}

/// Directory holding the environment for one specifier.
fn dlx_environment_root(spec: &str) -> PathBuf {
    otter_pm::user_cache_root()
        .join("dlx")
        .join(environment_key(spec))
}

/// Filesystem-safe key for a package specifier.
fn environment_key(spec: &str) -> String {
    let mut key = String::with_capacity(spec.len());
    for byte in spec.chars() {
        if byte.is_ascii_alphanumeric() || matches!(byte, '.' | '-' | '_') {
            key.push(byte);
        } else {
            key.push('_');
        }
    }
    key
}

/// Split `pkg@range` into its parts, keeping a scope's leading `@`.
fn split_package_spec(spec: &str) -> (String, String) {
    let search_from = usize::from(spec.starts_with('@'));
    match spec[search_from..].find('@') {
        Some(offset) => {
            let at = search_from + offset;
            (spec[..at].to_string(), spec[at + 1..].to_string())
        }
        None => (spec.to_string(), "*".to_string()),
    }
}

/// Binary name a package provides by default: its name without a scope.
fn binary_name(package: &str) -> String {
    package
        .rsplit('/')
        .next()
        .unwrap_or(package)
        .trim_start_matches('@')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_specifier_splits_into_package_and_range() {
        assert_eq!(
            split_package_spec("esbuild"),
            ("esbuild".to_string(), "*".to_string())
        );
        assert_eq!(
            split_package_spec("typescript@5.9"),
            ("typescript".to_string(), "5.9".to_string())
        );
        assert_eq!(
            split_package_spec("@scope/tool"),
            ("@scope/tool".to_string(), "*".to_string())
        );
        assert_eq!(
            split_package_spec("@scope/tool@^2"),
            ("@scope/tool".to_string(), "^2".to_string())
        );
    }

    #[test]
    fn a_scoped_package_runs_its_unscoped_binary() {
        assert_eq!(binary_name("esbuild"), "esbuild");
        assert_eq!(binary_name("@scope/tool"), "tool");
    }

    #[test]
    fn an_environment_key_is_filesystem_safe_and_specific() {
        assert_eq!(environment_key("@scope/tool@^2.0.0"), "_scope_tool__2.0.0");
        assert_ne!(
            environment_key("typescript@5.9"),
            environment_key("typescript@5.8")
        );
    }

    #[tokio::test]
    async fn an_installed_binary_is_found_from_a_nested_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("tool"), "#!/usr/bin/env otter\n").unwrap();
        let nested = tmp.path().join("packages/member");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            find_installing_root(&nested, "tool").await,
            Some(tmp.path().to_path_buf())
        );
        assert_eq!(find_installing_root(&nested, "absent").await, None);
    }
}
