//! Test262 conformance runner for the active Otter stack.
//!
//! This crate speaks the active `otter-runtime` / `otter-vm` ABI and
//! is the single source of truth for ECMA-262 conformance numbers
//! reported by the project.
//!
//! # Contents
//!
//! - [`runner`]          — corpus traversal + per-test driver.
//! - [`results`]         — sole owned rows and typed skip causes.
//! - [`metadata`]        — `/*--- ... ---*/` YAML frontmatter
//!   parser.
//! - [`harness`]         — `assert.js` / `sta.js` / `includes`
//!   loader.
//! - [`feature_map`]     — Test262 `features:` token →
//!   engine-readiness bucket.
//! - [`config`]          — `test262_config.toml` policy loader.
//! - [`provenance`]      — actual executable and effective policy identity.
//! - [`report`]          — complete JSON rows, validation and Markdown.
//! - [`site`]            — static HTML conformance dashboard.
//! - [`diff`]            — baseline diff.
//! - [`shard`]           — `--shard N/M` traversal.
//! - [`isolation`]       — fresh-runtime factory.
//!
//! # Invariants
//! Every canonical report retains all selected test paths exactly once.
//! Rollups derive from those rows; absent paths never imply a passing test.
//!
//! # See also
//! Spec links:
//! - <https://tc39.es/ecma262/>
//! - <https://github.com/tc39/test262/blob/main/INTERPRETING.md>

#![forbid(unsafe_code)]

pub mod agent;
pub mod config;
pub mod diff;
pub mod feature_map;
pub mod harness;
pub mod isolation;
pub mod metadata;
pub mod provenance;
pub mod report;
pub mod results;
pub mod runner;
pub mod shard;
pub mod site;

pub use runner::{CorpusError, CorpusPaths, count_tests, ensure_corpus_present, list_tests};
