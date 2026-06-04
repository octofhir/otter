//! Test262 conformance runner for the new-engine
//! (`crates/*`) Otter stack.
//!
//! This crate speaks the active `otter-runtime` / `otter-vm` ABI and
//! is the single source of truth for ECMA-262 conformance numbers
//! reported by the project.
//!
//! # Layout (target shape — slices 101 → 105)
//!
//! - [`runner`]          — corpus traversal + per-test driver.
//! - [`metadata`]        — `/*--- ... ---*/` YAML frontmatter
//!   parser (slice 102).
//! - [`harness`]         — `assert.js` / `sta.js` / `includes`
//!   loader (slice 102).
//! - [`feature_map`]     — Test262 `features:` token →
//!   engine-readiness bucket (slice 102).
//! - [`config`]          — `test262_config.toml` loader (the
//!   format the project has been on since the legacy runner).
//! - [`report`]          — JSON + Markdown writers (slice 104).
//! - [`site`]            — static HTML conformance dashboard.
//! - [`diff`]            — baseline diff (slice 104).
//! - [`shard`]           — `--shard N/M` traversal (slice 104).
//! - [`isolation`]       — fresh-runtime factory (slice 103).
//!
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
pub mod report;
pub mod runner;
pub mod shard;
pub mod site;

pub use runner::{CorpusError, CorpusPaths, count_tests, ensure_corpus_present, list_tests};
