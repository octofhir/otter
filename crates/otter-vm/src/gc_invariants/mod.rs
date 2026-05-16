//! Internal GC invariant tests.
//!
//! These tests intentionally exercise raw mark/sweep and root-walker cases from
//! inside the VM crate. Keeping them here lets the production crate hide
//! old-space setup helpers from downstream users while still testing GC edge
//! cases that do not run through a normal interpreter/native root contract.

mod array_cycles;
mod async_iterator_promise_roots;
mod callable_and_regexp_roots;
mod collection_cycles;
mod object_cycles;
mod root_enumeration;
mod weak_collection_ephemerons;
mod weak_refs_and_finalization;
