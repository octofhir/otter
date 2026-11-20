//! Tests of the public embedding API.
//!
//! # Contents
//! Bootstrap, scheduling, module loading and language behavior.
//!
//! # Invariants
//! Fixtures exercise their own runtime instances.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use super::*;

mod bootstrap;
mod language;
mod modules;
mod scheduling;
