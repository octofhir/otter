//! # Otter Node.js Compatibility Test Runner
//!
//! Runs Node.js test files against the Otter VM to measure
//! compatibility with Node.js built-in module APIs.

#![warn(clippy::all)]

pub mod compare;
pub mod config;
pub mod report;
pub mod runner;

pub use report::{FailureInfo, ModuleReport, PersistedReport, TestReport};
pub use runner::{NodeCompatRunner, TestOutcome, TestResult};
