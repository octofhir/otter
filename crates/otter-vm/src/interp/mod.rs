//! `Interpreter` inherent-method modules split out of `lib.rs`.
//!
//! Each file holds one cohesive `impl Interpreter` slice; `helpers` holds the
//! free functions that back the dispatch loop. No public-API change: `lib.rs`
//! re-exports the names that were previously defined at the crate root.
mod dispatch;
mod errors;
mod exec;
mod feedback;
mod frames;
pub(crate) mod helpers;
mod host;
mod init;
mod jit_call;
mod jit_compile;
mod modules;
mod protos;
mod shapes;
mod stats;
#[cfg(test)]
mod tests;
mod trace_roots;

pub(crate) use feedback::FeedbackDirectory;
