//! tsgo type checker integration via RPC.
//!
//! This module provides integration with tsgo (TypeScript's native Go compiler)
//! for high-performance type checking. tsgo is 10x faster than tsc.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     TypeChecker                              │
//! │  ┌─────────────────────────────────────────────────────────┐ │
//! │  │                   TsgoChannel                           │ │
//! │  │   ┌─────────────┐    JSON-RPC     ┌────────────────┐   │ │
//! │  │   │   Request   │ ──────────────► │  tsgo --api    │   │ │
//! │  │   │   (stdin)   │                 │   (subprocess) │   │ │
//! │  │   └─────────────┘                 └────────────────┘   │ │
//! │  │   ┌─────────────┐                         │            │ │
//! │  │   │  Response   │ ◄──────────────────────┘            │ │
//! │  │   │  (stdout)   │                                      │ │
//! │  │   └─────────────┘                                      │ │
//! │  └─────────────────────────────────────────────────────────┘ │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```no_run
//! use otter_runtime::tsgo::{TypeChecker, TypeCheckConfig};
//!
//! async fn check_project() {
//!     let mut checker = TypeChecker::new().await.unwrap();
//!     let config = TypeCheckConfig::default();
//!     let diagnostics = checker.check_project(
//!         std::path::Path::new("./tsconfig.json"),
//!         &config,
//!     ).unwrap();
//!
//!     for diag in &diagnostics {
//!         println!("{}: {}", diag.code, diag.message);
//!     }
//! }
//! ```

mod binary;
mod checker;
mod diagnostics;
mod rpc;

pub use binary::{
    cache_dir, download_tsgo, download_tsgo_blocking, find_tsgo, find_tsgo_blocking,
    is_tsgo_available, tsgo_version,
};
pub use checker::{TypeCheckConfig, TypeChecker, check_file, check_project, check_types};
pub use diagnostics::{
    Diagnostic, DiagnosticSeverity, Position, error_count, format_diagnostics,
    format_diagnostics_colored, has_errors, warning_count,
};
pub use rpc::TsgoChannel;
