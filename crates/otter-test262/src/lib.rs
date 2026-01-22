//! # Otter Test262 Runner
//!
//! Test262 conformance test runner for the Otter VM.
//!
//! Test262 is the official ECMAScript conformance test suite.
//! This crate runs these tests against our VM to measure compatibility.

#![warn(clippy::all)]

pub mod harness;
pub mod metadata;
pub mod report;
pub mod runner;

pub use report::{FeatureReport, TestReport};
pub use runner::{Test262Runner, TestOutcome, TestResult};
