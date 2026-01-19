//! Otter engine core.
//!
//! This crate provides the ESM module loader, dependency graph, and
//! capability-based security for the Otter JavaScript runtime.
//!
//! # Features
//!
//! - **ESM Module Loading**: Load ES modules from file://, node:, and https:// URLs
//! - **Security**: Capability-based permissions and allowlist for remote modules
//! - **Caching**: In-memory and disk caching for loaded modules
//! - **TypeScript**: Automatic transpilation of .ts/.tsx files
//! - **Import Maps**: Support for module aliasing
//! - **Dependency Graph**: Cycle detection and topological sorting
//!
//! # Example
//!
//! ```no_run
//! use otter_engine::{ModuleLoader, ModuleGraph, LoaderConfig};
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = LoaderConfig::default();
//!     let loader = Arc::new(ModuleLoader::new(config));
//!     let mut graph = ModuleGraph::new(loader);
//!
//!     // Load a module and its dependencies
//!     graph.load("file:///path/to/main.js").await?;
//!
//!     // Get execution order
//!     for url in graph.execution_order() {
//!         println!("Execute: {}", url);
//!     }
//!
//!     Ok(())
//! }
//! ```

pub mod capabilities;
pub mod env_store;
pub mod graph;
pub mod loader;

pub use capabilities::{Capabilities, CapabilitiesBuilder, PermissionDenied};
pub use env_store::{
    DEFAULT_DENY_PATTERNS, EnvFileError, EnvStoreBuilder, EnvWriteError, IsolatedEnvStore,
    parse_env_file,
};
pub use graph::{ImportRecord, ModuleGraph, ModuleNode, parse_imports};
pub use loader::{ImportContext, LoaderConfig, ModuleLoader, ModuleType, ResolvedModule, SourceType};

// Re-export error types from otter-runtime for convenience
pub use otter_runtime::{JscError, JscResult};
