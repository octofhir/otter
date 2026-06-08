//! `otter-regex` — a standalone, VM-agnostic ECMAScript RegExp engine.
//!
//! This crate is a clean-room reimplementation of the ECMAScript Regular
//! Expression grammar and matching semantics (ECMA-262 §22.2). It operates
//! purely on input slices (`&[u16]` code units in UTF-16, or `&str`) and
//! integer code-unit ranges. It has **zero dependency on the Otter VM, GC, or
//! any Otter runtime crate** — the dependency edge is one-way: a host VM
//! consumes this finished engine; the engine never reaches back into the host.
//!
//! # Contents
//! - [`Regex`] — a compiled pattern; the public entry point.
//! - [`Flags`] — engine-relevant flag bits (`i m s u v`). The stateful JS flags
//!   `g`/`y`/`d` live *above* the matcher in the host and are not modelled here.
//! - [`ExecConfig`] — per-execution tuning (the ReDoS step budget).
//! - [`Match`] — a single match: overall range, capture ranges, named groups.
//! - [`RegexError`] — pattern compile / early-error failure.
//! - [`ExecError`] — execution aborted under an [`ExecConfig`] constraint.
//!
//! # Invariants
//! - All offsets in a [`Match`] are **UTF-16 code-unit** indices, even in
//!   Unicode (`u`/`v`) mode where matching is code-point-aware. The host slices
//!   its `&[u16]` subject directly with these ranges.
//! - Capturing groups are 1-based in [`Match::captures`] (index 0 of the vector
//!   is group 1); the overall match (group 0) is [`Match::range`].
//! - Named-group iteration is deterministic (pattern source order).
//! - The parser enforces an explicit recursion depth limit; pathological nested
//!   groups/classes raise a [`RegexError`] rather than overflowing the stack.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-regexp-regular-expression-objects>
//! - `docs/regex-rewrite-research.md` — the architecture decision record.

// Milestone-1 scaffold: the parser, lowering, and executor bodies are `todo!()`
// until Milestone 2, so the internal types/fns they will consume are not yet
// referenced. Removed once M2 wires the pipeline end to end.
#![allow(dead_code)]

mod api;
mod casefold;
mod classes;
mod cursor;
mod error;
mod exec;
mod flags;
mod ir;
mod parser;
mod program;
mod unicode;

pub use api::{Match, Matches, NamedGroups, Regex};
pub use error::{ExecError, RegexError};
pub use exec::ExecConfig;
pub use flags::Flags;
