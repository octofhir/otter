//! Production-grade page-based generational tracing GC for the Otter
//! new-engine VM.
//!
//! This crate is the home of the tracing garbage collector that
//! replaces `Rc<RefCell<…>>` across `crates-next/otter-vm` value
//! types. The architecture is V8 Orinoco / JSC Riptide shaped as of
//! 2026 — page-based heap with a 4 GiB pointer-compression cage,
//! semispace young-gen scavenger, tri-color mark-sweep old-gen,
//! generational + Dijkstra insertion write barriers, type-tag
//! function-pointer trace dispatch, RAII handle scopes.
//!
//! # Status
//!
//! Foundation slice for task 71 — the crate skeleton lands here so
//! task 72 (`72-gc-core-heap-and-handles.md`) has a build target.
//! No GC implementation in this commit; only the crate exists with
//! its module-level docstring, Cargo metadata, and the
//! [`_placeholder`] sentinel.
//!
//! # Contents
//!
//! - [`_placeholder`] — sentinel function so the crate builds and
//!   participates in `cargo test --workspace` before task 72 lands
//!   any real types.
//!
//! # Invariants
//!
//! - `unsafe_code` is permitted only inside this crate (per ADR-0004,
//!   amending ADR-0001 §5). Every other `crates-next/*` crate keeps
//!   the workspace `forbid(unsafe_code)` ban.
//! - Every `unsafe` block landing in subsequent slices must carry a
//!   `// SAFETY:` comment; every public `unsafe fn` must document
//!   preconditions in a `# Safety` docstring section; every
//!   non-trivial unsafe block must have a corresponding
//!   `cargo +nightly miri test` regression.
//! - No path-dep on `crates/otter-gc/` — the legacy GC crate is
//!   design reference only (ADR-0001 §Working rules 1–2). All code
//!   here is rewritten under new conventions.
//!
//! # See also
//!
//! - [GC architecture plan](../../../docs/new-engine/gc-architecture.md)
//! - [ADR-0001 — staging directory](../../../docs/new-engine/adr/0001-staging-directory.md)
//! - [ADR-0004 — GC crate & unsafe boundary](../../../docs/new-engine/adr/0004-gc-crate-and-unsafe-boundary.md)
//! - [Task 71 — crate skeleton](../../../docs/new-engine/tasks/71-gc-crate-skeleton.md)
//! - [Task 72 — core heap and handles](../../../docs/new-engine/tasks/72-gc-core-heap-and-handles.md)

/// Sentinel placeholder so the crate builds and tests run before
/// task 72 lands real types. Removed in the first task-72 commit.
pub fn _placeholder() {
    // SAFETY: `mem::zeroed::<u8>()` is sound because `u8` admits the
    // all-zero bit pattern. The call is only here to ensure the
    // ADR-0004 lift of `forbid(unsafe_code)` takes effect on this
    // crate; if the lint override regresses, the build fails. The
    // sentinel is removed in the first task-72 commit when real
    // unsafe lands.
    let _: u8 = unsafe { core::mem::zeroed() };
}

#[cfg(test)]
mod tests {
    use super::_placeholder;

    #[test]
    fn placeholder_is_callable() {
        _placeholder();
    }
}
