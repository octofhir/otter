//! Otter package manager.
//!
//! This crate provides npm registry client, dependency resolution,
//! and package installation capabilities.

pub mod bin_resolver;
pub mod binary_lockfile;
pub mod content_store;
pub mod install;
pub mod lockfile;
pub mod manifest_cache;
pub mod progress;
pub mod registry;
pub mod resolver;
pub mod scripts;
pub mod types;

pub use bin_resolver::{BinResolver, ResolvedBin};
pub use binary_lockfile::{BinaryLockEntry, BinaryLockfile};
pub use content_store::{ContentStore, PackageIndex, StoredFile};
pub use install::{BinField, InstallError, Installer, PackageJson};
pub use lockfile::{Lockfile, LockfileEntry, LockfileError};
pub use manifest_cache::{CachedManifest, ManifestCache};
pub use progress::InstallProgress;
pub use registry::{DistInfo, NpmRegistry, PackageMetadata, RegistryError, VersionInfo};
pub use resolver::{ResolvedPackage, Resolver, ResolverError};
pub use scripts::{
    ScriptError, ScriptResult, ScriptRunner, find_package_json, format_scripts_list,
};
pub use types::{TypesError, install_bundled_types};

// Re-export the new split crates so downstream code can migrate
// incrementally without breaking the old `otter_pm::*` import paths.
// The existing types above (`Lockfile`, `PackageJson`, etc.) continue
// to work unchanged; the new ones live under `otter_pm::graph` /
// `otter_pm::manifest` so both can coexist during Phase 1.
pub use otter_pm_lockfile as graph;
pub use otter_pm_manifest as manifest;
