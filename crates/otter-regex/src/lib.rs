//! `otter-regex` — a standalone, VM-agnostic ECMAScript RegExp engine.
//!
//! This crate is a clean-room reimplementation of the ECMAScript Regular
//! Expression grammar and matching semantics (ECMA-262 §22.2). It operates
//! purely on input slices (`&[u16]` code units in UTF-16, or `&str`) and
//! integer code-unit ranges. It has **zero dependency on the Otter VM, GC, or
//! any Otter runtime crate** — the dependency edge is one-way: a host VM
//! consumes this finished engine; the engine never reaches back into the host.
//!
//! The default `runtime` feature includes matching and native compilation.
//! Host compilers disable it and use `validate_utf16` with the same parser.
//!
//! # Contents
//! - [`validate_utf16`] — syntax and early errors, without executable IR.
//! - `Regex` — a compiled pattern; the public entry point.
//! - `MatchScratch` — matcher buffers a host reuses across searches.
//! - [`Flags`] — engine-relevant flag bits (`i m s u v`). The stateful JS flags
//!   `g`/`y`/`d` live *above* the matcher in the host and are not modelled here.
//! - `ExecConfig` — per-execution tuning (the ReDoS step budget).
//! - `Match` — a borrowed match view with capture ranges and named groups.
//! - `MatchBatch` — owned flat offsets, independent of search scratch.
//! - `MatchRecord` — one owned result with direct access to its overall range.
//! - `NO_OFFSET` — the offset a non-participating group writes.
//! - [`RegexError`] — pattern compile / early-error failure.
//! - `ExecError` — execution aborted under an `ExecConfig` constraint.
//!
//! # Invariants
//! - All offsets in a `Match` are **UTF-16 code-unit** indices, even in
//!   Unicode (`u`/`v`) mode where matching is code-point-aware. The host slices
//!   its `&[u16]` subject directly with these ranges.
//! - Capturing groups are 1-based in `Match::captures` (index 0 of the view
//!   is group 1); the overall match (group 0) is `Match::range`.
//! - Named-group iteration is deterministic (pattern source order).
//! - The parser enforces an explicit recursion depth limit; pathological nested
//!   groups/classes raise a [`RegexError`] rather than overflowing the stack.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-regexp-regular-expression-objects>

#[cfg(feature = "runtime")]
mod api;
#[cfg(feature = "runtime")]
mod casefold;
mod classes;
#[cfg(feature = "runtime")]
mod cursor;
mod error;
#[cfg(feature = "runtime")]
mod exec;
mod flags;
#[cfg(feature = "runtime")]
mod ir;
#[cfg(feature = "runtime")]
mod matches;
mod parser;
#[cfg(feature = "runtime")]
mod program;
mod unicode;
mod validation;

pub use validation::validate_utf16;

#[cfg(feature = "runtime")]
pub use api::{MatchScratch, MatchSlots, Matches, Regex};
#[cfg(feature = "runtime")]
pub use cursor::Unit;
#[cfg(feature = "runtime")]
pub use error::ExecError;
pub use error::RegexError;
#[cfg(feature = "runtime")]
pub use exec::ExecConfig;
pub use flags::Flags;
#[cfg(feature = "runtime")]
pub use matches::{Captures, Match, MatchBatch, MatchRecord, NO_OFFSET, NamedGroups};
