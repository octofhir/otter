//! Install lifecycle script execution and its approval gate.
//!
//! Lifecycle scripts are the one point where installing a dependency executes
//! that dependency's code, so this module is where the installer decides
//! whether that happens at all. A dependency runs its `preinstall` /
//! `install` / `postinstall` hooks only when the project has approved that
//! package by name; the root package — the user's own project — always runs
//! its own.
//!
//! Every decision is recorded back into the lockfile's trust state, so the
//! lockfile documents what actually ran rather than what was merely present.
//!
//! # Contents
//! - [`LifecycleOutcome`] — what ran and what was held back.
//! - [`PendingBuild`] — one package whose scripts await review.
//! - [`run_install_lifecycle_scripts`] — the gated execution pass.
//!
//! # Invariants
//! - A dependency's scripts run only under [`BuildDecision::Approved`].
//! - The root package's own scripts always run; approval governs dependencies.
//! - Skipping is silent to the script but never silent to the user: every
//!   skipped package is reported with its hooks and its sniff findings.
//! - Trust state written to the lockfile reflects the decision this install
//!   made, so a later read does not have to re-derive it.
//!
//! # See also
//! - [`crate::policy`] for how the approval set is resolved.
//! - [`crate::script_sniff`] for the advisory findings attached to a report.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use otter_pm_lockfile::{LifecycleMetadata, Lockfile, TrustState};

use crate::policy::{BuildDecision, InstallPolicy};
use crate::script_sniff::{ScriptFinding, suspicious_script_findings};
use crate::{InstalledPackage, PackageManagerError};

/// One executed lifecycle script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleRun {
    /// Package id or root package label.
    pub package: String,
    /// Package name.
    pub name: String,
    /// Lifecycle stage.
    pub stage: String,
    /// Script command.
    pub script: String,
    /// Working directory.
    pub cwd: PathBuf,
}

/// One package whose install scripts were held back for review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBuild {
    /// Lockfile package id.
    pub package: String,
    /// Package name, the key an approval is written under.
    pub name: String,
    /// Resolved package version.
    pub version: String,
    /// Install lifecycle hooks the package declares, in execution order.
    pub hooks: Vec<String>,
    /// Advisory findings from scanning the package's script bodies.
    pub findings: Vec<ScriptFinding>,
}

/// Result of the gated lifecycle pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleOutcome {
    /// Scripts that ran, in execution order.
    pub runs: Vec<LifecycleRun>,
    /// Packages whose scripts were skipped because nobody approved them.
    pub pending: Vec<PendingBuild>,
}

/// List the packages in `lockfile` whose install scripts await review under
/// `policy`, without installing anything.
///
/// The root package is excluded: a project always runs its own scripts, so it
/// is never awaiting its own approval.
#[must_use]
pub fn pending_builds(lockfile: &Lockfile, policy: &InstallPolicy) -> Vec<PendingBuild> {
    lockfile
        .packages
        .iter()
        .filter(|(_, package)| {
            !package.lifecycle.scripts.is_empty()
                && package
                    .resolved
                    .as_ref()
                    .is_none_or(|source| source.reference != ".")
                && policy.build_decision(&package.name) == BuildDecision::Unreviewed
        })
        .map(|(id, package)| PendingBuild {
            package: id.clone(),
            name: package.name.clone(),
            version: package.version.clone(),
            hooks: lifecycle_stages(&package.lifecycle),
            findings: findings_for(&package.lifecycle),
        })
        .collect()
}

/// Run install lifecycle scripts under `policy`, recording each decision into
/// the lockfile's trust state.
pub(crate) async fn run_install_lifecycle_scripts(
    project_root: &Path,
    lockfile: &mut Lockfile,
    installed: &BTreeMap<String, InstalledPackage>,
    policy: &InstallPolicy,
) -> Result<LifecycleOutcome, PackageManagerError> {
    let mut outcome = LifecycleOutcome::default();
    for (package_id, installed_package) in installed {
        let Some(locked) = lockfile.packages.get(package_id) else {
            continue;
        };
        let name = locked.name.clone();
        let version = locked.version.clone();
        let lifecycle = locked.lifecycle.clone();
        if lifecycle.scripts.is_empty() {
            record_trust(lockfile, package_id, TrustState::Trusted);
            continue;
        }
        match policy.build_decision(&name) {
            BuildDecision::Approved => {
                record_trust(lockfile, package_id, TrustState::Trusted);
                outcome.runs.extend(
                    run_package_lifecycle(
                        project_root,
                        package_id,
                        &name,
                        &lifecycle,
                        &installed_package.installed_root,
                    )
                    .await?,
                );
            }
            BuildDecision::Refused => {
                record_trust(lockfile, package_id, TrustState::Disabled);
            }
            BuildDecision::Unreviewed => {
                record_trust(lockfile, package_id, TrustState::Untrusted);
                outcome.pending.push(PendingBuild {
                    package: package_id.clone(),
                    name,
                    version,
                    hooks: lifecycle_stages(&lifecycle),
                    findings: findings_for(&lifecycle),
                });
            }
        }
    }

    if let Some((root_id, root_package)) = lockfile
        .packages
        .iter()
        .find(|(_, package)| {
            package
                .resolved
                .as_ref()
                .is_some_and(|source| source.reference == ".")
        })
        .map(|(id, package)| (id.clone(), package.clone()))
    {
        record_trust(lockfile, &root_id, TrustState::Trusted);
        outcome.runs.extend(
            run_package_lifecycle(
                project_root,
                &root_id,
                &root_package.name,
                &root_package.lifecycle,
                project_root,
            )
            .await?,
        );
    }
    Ok(outcome)
}

fn record_trust(lockfile: &mut Lockfile, package_id: &str, trust: TrustState) {
    if let Some(locked) = lockfile.packages.get_mut(package_id) {
        locked.lifecycle.trust = trust;
    }
}

fn lifecycle_stages(lifecycle: &LifecycleMetadata) -> Vec<String> {
    if lifecycle.hooks.is_empty() {
        lifecycle.scripts.keys().cloned().collect()
    } else {
        lifecycle.hooks.clone()
    }
}

fn findings_for(lifecycle: &LifecycleMetadata) -> Vec<ScriptFinding> {
    let mut findings = Vec::new();
    for script in lifecycle.scripts.values() {
        for finding in suspicious_script_findings(script) {
            if !findings.contains(&finding) {
                findings.push(finding);
            }
        }
    }
    findings
}

async fn run_package_lifecycle(
    project_root: &Path,
    package_id: &str,
    package_name: &str,
    lifecycle: &LifecycleMetadata,
    cwd: &Path,
) -> Result<Vec<LifecycleRun>, PackageManagerError> {
    let mut runs = Vec::new();
    for stage in lifecycle_stages(lifecycle) {
        let Some(script) = lifecycle.scripts.get(&stage) else {
            continue;
        };
        run_lifecycle_script(project_root, package_id, &stage, script, cwd).await?;
        runs.push(LifecycleRun {
            package: package_id.to_string(),
            name: package_name.to_string(),
            stage,
            script: script.clone(),
            cwd: cwd.to_path_buf(),
        });
    }
    Ok(runs)
}

async fn run_lifecycle_script(
    project_root: &Path,
    package_id: &str,
    stage: &str,
    script: &str,
    cwd: &Path,
) -> Result<(), PackageManagerError> {
    let mut command = lifecycle_shell_command(script);
    command.current_dir(cwd);
    command.env("INIT_CWD", project_root);
    command.env("OTTER_SCRIPT_SRC_DIR", cwd);
    command.env("npm_lifecycle_event", stage);
    command.env("npm_lifecycle_script", script);
    command.env("PATH", lifecycle_path(project_root, cwd));
    let status = command
        .status()
        .await
        .map_err(|err| PackageManagerError::Lifecycle {
            package: package_id.to_string(),
            stage: stage.to_string(),
            message: err.to_string(),
        })?;
    if !status.success() {
        return Err(PackageManagerError::Lifecycle {
            package: package_id.to_string(),
            stage: stage.to_string(),
            message: status.code().map_or_else(
                || "terminated by signal".to_string(),
                |code| format!("exit status {code}"),
            ),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn lifecycle_shell_command(script: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("cmd");
    command.arg("/C").arg(script);
    command
}

#[cfg(not(windows))]
fn lifecycle_shell_command(script: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(script);
    command
}

fn lifecycle_path(project_root: &Path, cwd: &Path) -> OsString {
    let separator = if cfg!(windows) { ";" } else { ":" };
    let mut paths = vec![
        cwd.join("node_modules").join(".bin").into_os_string(),
        project_root
            .join("node_modules")
            .join(".bin")
            .into_os_string(),
    ];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.push(existing);
    }
    let mut joined = OsString::new();
    for (index, path) in paths.into_iter().enumerate() {
        if index > 0 {
            joined.push(separator);
        }
        joined.push(path);
    }
    joined
}
