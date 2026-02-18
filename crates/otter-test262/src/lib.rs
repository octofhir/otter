//! # Otter Test262 Runner
//!
//! Test262 conformance test runner for the Otter VM.
//!
//! Test262 is the official ECMAScript conformance test suite.
//! This crate runs these tests against our VM to measure compatibility.

#![warn(clippy::all)]

pub mod compare;
pub mod config;
pub mod editions;
pub mod harness;
pub mod metadata;
pub mod parallel;
pub mod report;
pub mod runner;

pub use metadata::ExecutionMode;
pub use report::{FailureInfo, FeatureReport, PersistedReport, RunSummary, TestReport};
pub use runner::{Test262Runner, TestOutcome, TestResult};
