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
//! - [`store`] — safe write-barrier child enumeration for VM stores.
//! - [`marking`] — tri-color worklist.
//! - [`scavenger`] — Cheney BFS scavenger.
//! - [`barrier`] — write barriers.
//! - [`handle`] — `Local`, `HandleScope`, and internal persistent
//!   roots.
//! - [`branded`] — experimental isolate-branded session/root API.
//! - [`heap`] — `GcHeap` orchestrator.
//! - [`external`] — RAII accounting for native backing stores.
//! - [`extra_roots`] — heap-registered callbacks for runtime-owned roots.
//! - [`frame_roots`] — active interpreter frame-stack root registry.
//! - [`finalize`] — raw weak-reference and finalization bookkeeping.
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
//!   GC API/unsafe-boundary docs); every other `crates/*` crate keeps the
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
//! - [GC API](../../../docs/book/src/engine/gc-api.md)
//! - [Runtime architecture](../../../docs/book/src/engine/architecture.md)

/// Object alignment used everywhere in the GC. Every payload
/// starts at a multiple of this; cell size in bump alloc is the
/// same.
pub const OBJECT_ALIGNMENT: usize = 8;

#[doc(hidden)]
pub mod barrier;
pub mod branded;
#[doc(hidden)]
pub mod compressed;
pub mod devtools_snapshot;
#[doc(hidden)]
pub mod ephemeron;
pub mod external;
pub mod extra_roots;
#[doc(hidden)]
pub mod finalize;
pub mod frame_roots;
pub mod handle;
pub mod header;
pub mod heap;
#[doc(hidden)]
pub mod marking;
pub mod oom;
pub mod page;
pub mod root_scope;
#[doc(hidden)]
pub mod scavenger;
pub mod snapshot;
pub mod space;
pub mod stats;
pub mod store;
pub mod test_support;
#[doc(hidden)]
pub mod trace;

pub use branded::{GcSession, MutationSession, Root, Weak, with_gc_session};
pub use compressed::{CageStats, Gc, cage_base, cage_size, cage_stats, init_cage_with_size};
pub use external::{ExternalMemory, SharedExternalMemory};
pub use extra_roots::{ExtraRootSource, ExtraRoots};
pub use frame_roots::{FrameRootProviders, FrameRoots, RawFrameRoots};
pub use handle::{EscapableHandleScope, HandleScope, HandleStack, Local};
pub use header::{GcHeader, MarkColor};
pub use heap::{EmptyRoots, GcHeap, HeapStats};
pub use oom::OutOfMemory;
pub use page::{CARD_SIZE, PAGE_SIZE, Page, SpaceKind};
pub use root_scope::{ErasedSlotTracer, RootScope};
pub use snapshot::{HeapSnapshot, SnapshotObject};
pub use stats::{GcStats, TYPE_TAG_COUNT, TypeStats};
pub use store::{GcEdge, GcStore};
pub use trace::{SafeFinalize, SafeTraceable, Traceable};

/// Raw collector backend types used by audited VM adapter layers.
///
/// Normal builtin/native/module authors should not import this module.
/// Use [`Gc`], [`Local`], [`Root`], [`Weak`], [`GcStore`], and
/// context methods such as `NativeCtx::record_write` instead.
#[doc(hidden)]
pub mod raw {
    pub use crate::compressed::RawGc;
    pub use crate::heap::RootSlotVisitor;
    pub use crate::trace::{SlotVisitor, TraceFn, TraceTable};
}

// ---------------------------------------------------------------------------
// `!Send + !Sync` static assertions.
//
// Every GC primitive is bound to a single mutator thread. These compile-time
// checks make the single-mutator invariant visible to the type system: any
// future edit that accidentally adds `Send`/`Sync` to one of these handles will
// fail to compile, and `tokio::spawn` callers cannot capture them in `Send`
// futures (see compile-fail fixtures under
// `crates/otter-vm/tests/compile_fail/`).
//
// Spec:
// - <https://tc39.es/ecma262/#sec-agents> (one mutator per agent)
// ---------------------------------------------------------------------------
static_assertions::assert_not_impl_any!(GcHeap: Send, Sync);
static_assertions::assert_not_impl_any!(Gc<()>: Send, Sync);
static_assertions::assert_not_impl_any!(Local<'static, ()>: Send, Sync);
static_assertions::assert_not_impl_any!(HandleScope<'static>: Send, Sync);
