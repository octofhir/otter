//! Production-grade page-based generational tracing GC for the Otter
//! new-engine VM.
//!
//! Page-based heap with a 4 GiB pointer-compression cage, semispace
//! young-gen scavenger (Cheney BFS), tri-color mark-sweep old-gen,
//! generational + Dijkstra-insertion write barriers, type-tag
//! function-pointer trace dispatch, RAII handle scopes, Chrome
//! DevTools `.heapsnapshot` export. V8 Orinoco / JSC Riptide
//! shaped, 2026.
//!
//! # Contents
//!
//! - [`compressed`] — pointer compression: cage + `Gc<T>`.
//! - [`header`] — 8-byte `GcHeader`.
//! - [`page`] — 256 KiB pages, card table.
//! - [`space`] — `NewSpace`, `OldSpace`, `LargeObjectSpace`.
//! - [`trace`] — `TraceTable`, `Traceable` trait.
//! - [`marking`] — tri-color worklist.
//! - [`scavenger`] — Cheney BFS scavenger.
//! - [`barrier`] — write barriers.
//! - [`handle`] — `Local`, `HandleScope`, `GlobalHandle`.
//! - [`heap`] — `GcHeap` orchestrator.
//! - [`oom`] — `OutOfMemory` error.
//! - [`stats`] — per-heap counters and per-type rows.
//! - [`snapshot`] — Rust-side heap snapshot + retained-size walker.
//! - [`test_support`] — public Traceable helpers for downstream
//!   tests that keep `forbid(unsafe_code)`.
//! - [`devtools_snapshot`] — Chrome `.heapsnapshot` writer.
//!
//! # Invariants
//!
//! - `unsafe_code` is permitted only inside this crate (per
//!   ADR-0004); every other `crates-next/*` crate keeps the
//!   workspace `forbid(unsafe_code)`.
//! - Every `unsafe` block carries a `// SAFETY:` comment; every
//!   public `unsafe fn` documents preconditions in a `# Safety`
//!   docstring section.
//! - Pointer compression: every `Gc<T>` is a `u32` offset within a
//!   single process-global cage; `Gc::null()` decompresses to the
//!   reserved page-0 area, never to a real allocation.
//! - One isolate = one thread; `GcHeap` is `!Sync`, the cage is
//!   shared across heaps in the same process.
//!
//! # See also
//!
//! - [GC architecture plan](../../../docs/new-engine/gc-architecture.md)
//! - [ADR-0001](../../../docs/new-engine/adr/0001-staging-directory.md)
//!   — staging directory
//! - [ADR-0004](../../../docs/new-engine/adr/0004-gc-crate-and-unsafe-boundary.md)
//!   — GC crate & unsafe boundary
//! - Task 72 — core heap and handles.

/// Object alignment used everywhere in the GC. Every payload
/// starts at a multiple of this; cell size in bump alloc is the
/// same.
pub const OBJECT_ALIGNMENT: usize = 8;

pub mod barrier;
pub mod compressed;
pub mod devtools_snapshot;
pub mod handle;
pub mod header;
pub mod heap;
pub mod marking;
pub mod oom;
pub mod page;
pub mod scavenger;
pub mod snapshot;
pub mod space;
pub mod stats;
pub mod test_support;
pub mod trace;

pub use compressed::{Gc, RawGc, cage_base, cage_size, init_cage_with_size};
pub use handle::{GlobalHandle, GlobalHandleTable, HandleScope, HandleStack, Local};
pub use header::{GcHeader, MarkColor};
pub use heap::{EmptyRoots, GcHeap, HeapStats, Roots};
pub use oom::OutOfMemory;
pub use page::{CARD_SIZE, PAGE_SIZE, Page, SpaceKind};
pub use snapshot::{HeapSnapshot, SnapshotObject};
pub use stats::{GcStats, TYPE_TAG_COUNT, TypeStats};
pub use trace::{SlotVisitor, TraceFn, TraceTable, Traceable};
