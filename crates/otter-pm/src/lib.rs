//! Otter package manager.
//!
//! This crate provides npm registry client, dependency resolution,
//! and package installation capabilities.

pub mod install;
pub mod lockfile;
pub mod registry;
pub mod resolver;
pub mod types;

pub use install::{InstallError, Installer, PackageJson};
pub use lockfile::{Lockfile, LockfileEntry, LockfileError};
pub use registry::{DistInfo, NpmRegistry, PackageMetadata, RegistryError, VersionInfo};
pub use resolver::{ResolvedPackage, Resolver, ResolverError};
pub use types::{TypesError, install_bundled_types};
