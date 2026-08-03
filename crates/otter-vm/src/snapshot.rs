//! Isolate snapshot capture — the writer half of "don't run the
//! bootstrap, restore its result".
//!
//! A built isolate's durable state splits three ways:
//!
//! - **The heap image.** Old-space pages, captured verbatim by
//!   [`otter_gc::GcHeap::capture_old_space`]; every body in them is
//!   self-contained (the audit in `otter-gc/src/self_contained.rs`
//!   enforces zero escaping references and zero foreign ownership for
//!   the bootstrap graph).
//! - **Fixed roots.** Interpreter fields holding heap handles whose
//!   count and order are the same on every isolate —
//!   [`crate::Interpreter::visit_snapshot_roots`] walks them; the
//!   capture stores the sequence, a restore writes it back relocated.
//! - **Keyed side state.** Isolate-local structures the heap references
//!   by identifier rather than by handle: the property-name atom table
//!   (ids are baked into shapes and executable code), the global
//!   lexicals (name → cell), and — future work — per-type records for
//!   the payloads that cannot ride a page image at all (host objects,
//!   compiled regexps).
//!
//! `ShapeRuntime`'s side tables are deliberately *not* captured: they
//! are caches over heap state (`trace_roots` shows exactly which
//! cells), and a restore rebuilds them from the restored shape bodies.
//!
//! # Invariants
//!
//! - Capture requires an empty nursery; a full build ends with one, and
//!   the underlying page capture rejects anything else.
//! - `atom_names` is the interner's table in id order: restoring the
//!   list re-mints identical [`crate::property_atom::AtomId`]s.
//! - `global_lexicals` is sorted by name so two captures of the same
//!   build compare equal.
//!
//! # See also
//!
//! - `otter-gc/src/heap_image.rs` — pages and relocation.
//! - `scratchpad/PLAN_BOOTSTRAP_SNAPSHOT.md` — the plan this executes.

use otter_gc::raw::RawGc;

/// One dynamic-native closure carried by the in-process snapshot,
/// keyed by the host-ref index the captured bodies name it with.
pub(crate) enum DynamicNativePayload {
    /// `Send + Sync` embedder/runtime closure.
    Shared(std::sync::Arc<crate::native_function::NativeFn>),
    /// Isolate-local VM helper closure.
    Local(std::sync::Arc<crate::native_function::LocalNativeFn>),
}

/// Everything a restore needs that the page image alone does not carry.
pub struct IsolateSnapshot {
    /// The old generation, verbatim.
    pub image: otter_gc::HeapImage,
    /// The isolate's code space, shared by reference. Restored closure
    /// bodies name their bytecode by function id inside it. In-process
    /// only; the build-time snapshot pairs with embedded bytecode.
    pub(crate) code_space: std::sync::Arc<crate::code_space::CodeSpace>,
    /// Dynamic-native closures at their host-ref indices.
    pub(crate) dynamic_natives: Vec<(u32, DynamicNativePayload)>,
    /// Heap handles from the fixed-shape root walk, in walk order.
    pub fixed_roots: Vec<RawGc>,
    /// The property-name atom table, in id order.
    pub atom_names: Vec<Box<str>>,
    /// Global lexical bindings: name, cell handle, `is_const`.
    pub global_lexicals: Vec<(Box<str>, RawGc, bool)>,
}

impl crate::Interpreter {
    /// Capture everything a restore needs from this isolate.
    ///
    /// # Errors
    /// Propagates [`otter_gc::ImageError`]; a fresh build leaves the
    /// nursery empty, so a capture taken right after one succeeds.
    pub fn capture_isolate_snapshot(&self) -> Result<IsolateSnapshot, otter_gc::ImageError> {
        let image = self.capture_heap_image()?;
        let code_space = self.snapshot_code_space();
        let dynamic_natives = crate::native_function::snapshot_dynamic_natives(self.gc_heap());
        let fixed_roots = self.capture_snapshot_roots();
        let atom_names = self.snapshot_atom_names();
        let mut global_lexicals: Vec<(Box<str>, RawGc, bool)> = self
            .snapshot_global_lexicals()
            .into_iter()
            .map(|(name, cell, is_const)| (name, RawGc(cell.offset()), is_const))
            .collect();
        global_lexicals.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(IsolateSnapshot {
            image,
            code_space,
            dynamic_natives,
            fixed_roots,
            atom_names,
            global_lexicals,
        })
    }
}
