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
pub use scripts::{ScriptError, ScriptResult, ScriptRunner, find_package_json, format_scripts_list};
pub use types::{TypesError, install_bundled_types};
