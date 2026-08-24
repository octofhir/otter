//! Opaque in-process isolate snapshot capture.
//!
//! A built isolate's captured state splits three ways:
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
//!   Iterator and function-kind prototype caches are captured in their own
//!   fixed-size root arrays so restore can install them without allocation.
//! - **Keyed side state.** Isolate-local structures the heap references
//!   by identifier rather than by handle: the property-name atom table
//!   (ids are baked into shapes and executable code), the global
//!   lexicals (name → cell), and — future work — per-type records for
//!   the payloads that cannot ride a page image at all (host objects and
//!   compiled regexps).
//!
//! `ShapeRuntime`'s side tables are deliberately *not* captured: they
//! are caches over heap state (`trace_roots` shows exactly which
//! cells), and a restore rebuilds them from the restored shape bodies.
//! The carrier has no byte representation and cannot cross a process,
//! executable, or Rust type-layout boundary.
//!
//! # Contents
//!
//! - [`IsolateSnapshot`] — the opaque capture-produced carrier.
//! - [`crate::Interpreter::capture_isolate_snapshot`] — capture entry point.
//! - [`crate::Interpreter::from_isolate_snapshot`] — safe restore entry point.
//!
//! # Invariants
//!
//! - Capture requires an empty nursery; a full build ends with one, and
//!   the underlying page capture rejects anything else.
//! - `atom_names` is the interner's table in id order: restoring the
//!   list re-mints identical [`crate::property_atom::AtomId`]s.
//! - `global_lexicals` is sorted by name so two captures of the same
//!   build compare equal.
//! - All restoration-capable fields are crate-private. Public methods expose
//!   owned-data diagnostics only, never raw heap handles or mutable carriers.
//!
//! # See also
//!
//! - `otter-gc/src/heap_image.rs` — pages and relocation.
//! - `otter-gc/src/external_refs.rs` — process-local static addresses.

use otter_gc::raw::RawGc;

pub(crate) const ITERATOR_PROTOTYPE_ROOT_COUNT: usize = 14;

/// One dynamic-native closure carried by a snapshot, keyed by the
/// host-ref index the captured bodies name it with.
#[derive(Clone)]
pub(crate) enum DynamicNativePayload {
    /// `Send + Sync` embedder/runtime closure.
    Shared(std::sync::Arc<crate::native_function::NativeFn>),
    /// Isolate-local VM helper closure.
    Local(std::sync::Arc<crate::native_function::LocalNativeFn>),
}

/// Everything an in-process restore needs beyond the page image.
///
/// Instances can only be produced by [`crate::Interpreter`]. The fields remain
/// private outside the VM so embedders cannot forge raw roots, page contents,
/// or side-state records.
pub struct IsolateSnapshot {
    /// The old generation, verbatim.
    pub(crate) image: otter_gc::HeapImage,
    /// The isolate's code space, shared by reference. Restored closure
    /// bodies name their bytecode by function id inside it. In-process
    /// only.
    pub(crate) code_space: std::sync::Arc<crate::code_space::CodeSpace>,
    /// Dynamic-native closures at their host-ref indices.
    pub(crate) dynamic_natives: Vec<(u32, DynamicNativePayload)>,
    /// Heap handles from the fixed-shape root walk, in walk order.
    pub(crate) fixed_roots: Vec<RawGc>,
    /// Active and default-realm iterator prototype handles, in field order.
    pub(crate) iterator_prototype_roots: [RawGc; ITERATOR_PROTOTYPE_ROOT_COUNT],
    /// Function-kind constructor/prototype handles, in field order.
    pub(crate) function_kind_prototype_roots:
        [RawGc; crate::function_kind::FunctionKindPrototypes::SNAPSHOT_ROOT_COUNT],
    /// Every same-process external-reference address in index order, so a
    /// restore rebuilds the table with identical indices.
    pub(crate) external_ref_addrs: Vec<usize>,
    /// The property-name atom table, in id order.
    pub(crate) atom_names: Vec<Box<str>>,
    /// Global lexical bindings: name, cell handle, `is_const`.
    pub(crate) global_lexicals: Vec<(Box<str>, RawGc, bool)>,
    /// Per-body regexp pattern records in live-walk order. These rebuild the
    /// compiled matcher and foreign-owned text after page relocation.
    pub(crate) regexp_payloads: Vec<(Vec<u16>, String)>,
    /// Per-body array-sidecar descriptor-flag records in live-walk
    /// order: `(key, writable, enumerable, configurable)` per entry.
    pub(crate) array_sidecar_flags: Vec<Vec<(String, bool, bool, bool)>>,
    /// The source isolate's next shape id. A restoring isolate bumps
    /// the process counter past this so freshly minted shape ids never
    /// collide with the image's — shape ids key the property caches.
    pub(crate) next_shape_id: u64,
}

impl IsolateSnapshot {
    /// Borrow captured atom names in atom-id order.
    #[must_use]
    pub fn atom_names(&self) -> &[Box<str>] {
        &self.atom_names
    }

    /// Number of handles captured by the fixed root walk.
    #[must_use]
    pub const fn fixed_root_count(&self) -> usize {
        self.fixed_roots.len()
    }

    /// Iterate global lexical names in deterministic name order.
    pub fn global_lexical_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.global_lexicals
            .iter()
            .map(|(name, _, _)| name.as_ref())
    }

    /// Number of heap bodies captured in the image.
    #[must_use]
    pub fn object_count(&self) -> u64 {
        self.image.object_count()
    }
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
        let mut iterator_prototype_roots = [RawGc::NULL; ITERATOR_PROTOTYPE_ROOT_COUNT];
        for (slot, object) in iterator_prototype_roots[..7].iter_mut().zip([
            self.array_iterator_prototype.get(),
            self.map_iterator_prototype.get(),
            self.set_iterator_prototype.get(),
            self.string_iterator_prototype.get(),
            self.regexp_string_iterator_prototype.get(),
            self.iterator_helper_prototype.get(),
            self.wrap_for_valid_iterator_prototype.get(),
        ]) {
            *slot = object.map_or(RawGc::NULL, crate::object::JsObject::raw);
        }
        for (slot, root) in iterator_prototype_roots[7..]
            .iter_mut()
            .zip(&self.default_realm_iterator_prototypes)
        {
            *slot = root.get().map_or(RawGc::NULL, crate::object::JsObject::raw);
        }
        let function_kind_prototype_roots = self.function_kind_prototypes.snapshot_roots();
        let external_ref_addrs = self.gc_heap().external_refs().addresses().to_vec();
        let mut regexp_payloads: Vec<(Vec<u16>, String)> = Vec::new();
        self.gc_heap()
            .for_each_live_payload::<crate::regexp::JsRegExpBody, _>(|_space, body| {
                regexp_payloads.push((body.pattern_utf16.clone(), body.source.clone()));
            });
        let mut array_sidecar_with_content = false;
        let mut array_sidecar_flags: Vec<Vec<(String, bool, bool, bool)>> = Vec::new();
        self.gc_heap()
            .for_each_live_payload::<crate::array::ArrayExoticSlots, _>(|_space, body| {
                if !body.is_empty_for_snapshot() {
                    array_sidecar_with_content = true;
                    eprintln!(
                        "snapshot capture: array sidecar holds {}",
                        body.snapshot_content_summary()
                    );
                }
                array_sidecar_flags.push(body.snapshot_property_flags());
            });
        if array_sidecar_with_content {
            return Err(otter_gc::ImageError::ForeignPayloadNotCapturable {
                type_name: "ArrayExoticSlots",
            });
        }
        let next_shape_id = crate::object::snapshot_next_shape_id();
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
            iterator_prototype_roots,
            function_kind_prototype_roots,
            external_ref_addrs,
            atom_names,
            global_lexicals,
            regexp_payloads,
            array_sidecar_flags,
            next_shape_id,
        })
    }
}
