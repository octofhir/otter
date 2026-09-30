//! Per-scope binding contexts: the heap storage of every captured or
//! eval-visible binding.
//!
//! A context is one scope's bindings (§9.1 Environment Records) laid out as a
//! fixed slot array. The compiler decides statically which scopes own a
//! context and which slot each binding occupies; bytecode reaches a binding as
//! a register holding a context plus a packed
//! [`otter_bytecode::ContextCoord`] (`depth` parent hops, then `slot`). A
//! closure keeps exactly the one context it was created over; every outer
//! binding its body reaches is a hop from there.
//!
//! # Contents
//! - [`ContextBody`] — the GC body: scope identity and parent, then the
//!   trailing slot array and, for a scope that can receive one, the eval
//!   extension word after the last slot.
//! - [`ContextHandle`] — its compressed handle.
//! - Byte offsets of every field, published to generated code through
//!   [`crate::jit::JitContextLayout`].
//! - Allocation (`alloc_context_with_roots`, `copy_context_with_roots`),
//!   chain walking, slot access, and the eval-extension probe used by the
//!   `Lookup*` operations.
//!
//! # Invariants
//! - `parent` holds a context value or `undefined`; the extension word, which
//!   only a scope whose descriptor has `has_extension` (V8's `extension`
//!   slot, present only for sloppy-eval scopes) carries, holds an
//!   `EvalExtensionBody` value or `undefined`. Both are full 8-byte `Value`
//!   words, so generated code dereferences them with one load and no
//!   cage-base arithmetic. Slot offsets never depend on the extension.
//! - The body and its whole slot array are initialized before the cell can be
//!   observed by any collection: allocation goes through
//!   [`otter_gc::GcHeap::alloc_trailing_with_roots_initialized`], whose
//!   initializer runs at the final address before any safepoint. A pending
//!   (stack-resident) body traces only `parent`.
//! - Contexts are allocated young, or old under bootstrap tenuring. Every
//!   store into a slot or the extension field after allocation records the
//!   write barrier; no store elides it, because a context may sit in old
//!   space (tenuring or nursery overflow) whatever its age.
//! - `(scope_function_id, scope_index)` names the descriptor in
//!   `CodeBlock::scopes` that fixes the slot names and kinds. A live context
//!   reports `scope_function_id` to code liveness, so the owning chunk (and
//!   its descriptor table) is never evicted under it.
//! - A context is an internal value: never an ECMAScript Object, never
//!   `typeof`-observable, never recorded by type feedback.
//! - A slot holding [`Value::hole`] is a binding in its temporal dead zone.
//!
//! # See also
//! - `crate::eval_env` — the eval extension a sloppy direct eval adds.
//! - `crate::context_ops` — the interpreter kernels over contexts.
//! - `benchmarks/measurements/2026-09-27-environments-contract.md`.

use otter_gc::GcHeap;
use otter_gc::OutOfMemory;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::SlotVisitor;

use crate::Value;
use crate::eval_env::EvalExtensionHandle;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ContextBody`].
pub const CONTEXT_BODY_TYPE_TAG: u8 = 0x21;

/// GC body of one scope's context. The slot array follows the fixed fields in
/// the same cell.
#[repr(C, align(8))]
#[derive(Debug)]
pub struct ContextBody {
    /// Global VM function id of the function whose `CodeBlock::scopes` holds
    /// this context's descriptor.
    pub(crate) scope_function_id: u32,
    /// Index of the descriptor in that function's scope table, with
    /// [`CONTEXT_HAS_EXTENSION`] set when the cell carries the eval-extension
    /// word after its slots.
    pub(crate) scope_index: u16,
    /// Number of trailing slots.
    pub(crate) slot_count: u16,
    /// Enclosing context, or `undefined` for the outermost.
    pub(crate) parent: Value,
}

/// [`ContextBody::scope_index`] bit marking a context with an eval-extension
/// word. Scope tables never reach this index.
pub(crate) const CONTEXT_HAS_EXTENSION: u16 = 1 << 15;

/// Compressed handle to a [`ContextBody`].
pub type ContextHandle = otter_gc::Gc<ContextBody>;

/// Byte offset of `scope_function_id` in the payload.
pub const CONTEXT_BODY_SCOPE_FUNCTION_ID_OFFSET: usize =
    std::mem::offset_of!(ContextBody, scope_function_id);
/// Byte offset of `scope_index` in the payload.
pub const CONTEXT_BODY_SCOPE_INDEX_OFFSET: usize = std::mem::offset_of!(ContextBody, scope_index);
/// Byte offset of `slot_count` in the payload.
pub const CONTEXT_BODY_SLOT_COUNT_OFFSET: usize = std::mem::offset_of!(ContextBody, slot_count);
/// Byte offset of `parent` in the payload.
pub const CONTEXT_BODY_PARENT_OFFSET: usize = std::mem::offset_of!(ContextBody, parent);
/// Byte offset of slot 0 in the payload; slot `i` is `8 * i` further.
pub const CONTEXT_BODY_SLOTS_OFFSET: usize = std::mem::size_of::<ContextBody>();

const _: [(); 0] = [(); CONTEXT_BODY_SCOPE_FUNCTION_ID_OFFSET];
const _: [(); 4] = [(); CONTEXT_BODY_SCOPE_INDEX_OFFSET];
const _: [(); 6] = [(); CONTEXT_BODY_SLOT_COUNT_OFFSET];
const _: [(); 8] = [(); CONTEXT_BODY_PARENT_OFFSET];
const _: [(); 16] = [(); CONTEXT_BODY_SLOTS_OFFSET];
const _: [(); 8] = [(); std::mem::align_of::<ContextBody>()];

impl otter_gc::SafeTraceable for ContextBody {
    const TYPE_TAG: u8 = CONTEXT_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        self.parent.trace_value_slot_mut(visitor);
        let base = self.slots_ptr();
        for index in 0..self.traced_word_count() {
            // SAFETY: an allocated context owns exactly `slot_count`
            // initialized slots after the fixed body; the visitor rewrites
            // the slot in place.
            unsafe { (*base.add(index)).trace_value_slot_mut(visitor) };
        }
    }

    /// A pending body on the allocator's stack has no slot array or
    /// extension word yet; only its parent exists.
    fn trace_pending_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        self.parent.trace_value_slot_mut(visitor);
    }
}

impl ContextBody {
    fn slots_ptr(&self) -> *mut Value {
        // SAFETY: computes the tail address only; dereferenced solely on an
        // allocated body with `slot_count` initialized slots.
        unsafe { (self as *const Self).add(1).cast_mut().cast::<Value>() }
    }

    /// The initialized slot array.
    pub(crate) fn slots(&self) -> &[Value] {
        // SAFETY: allocated contexts initialize exactly `slot_count` slots
        // before publication (see the module invariants).
        unsafe { std::slice::from_raw_parts(self.slots_ptr(), self.slot_count as usize) }
    }

    /// Set the extension word of a freshly allocated cell to `undefined`.
    fn initialize_extension_word(&mut self) {
        if self.has_extension_word() {
            // SAFETY: the cell owns the extension word after its slots.
            unsafe { *self.slots_ptr().add(self.slot_count as usize) = Value::undefined() };
        }
    }

    fn slots_mut(&mut self) -> &mut [Value] {
        // SAFETY: as `slots`; the exclusive borrow covers the whole cell.
        unsafe { std::slice::from_raw_parts_mut(self.slots_ptr(), self.slot_count as usize) }
    }

    /// Parent context handle, `None` for the outermost.
    pub(crate) fn parent_handle(&self) -> Option<ContextHandle> {
        self.parent.as_context()
    }

    /// Whether the cell carries the eval-extension word.
    pub(crate) fn has_extension_word(&self) -> bool {
        self.scope_index & CONTEXT_HAS_EXTENSION != 0
    }

    /// Descriptor index in the owning function's scope table.
    pub(crate) fn scope_index(&self) -> u16 {
        self.scope_index & !CONTEXT_HAS_EXTENSION
    }

    /// Slots plus the extension word, when present.
    fn traced_word_count(&self) -> usize {
        self.slot_count as usize + usize::from(self.has_extension_word())
    }

    /// The eval-extension word, `undefined` while no sloppy direct eval has
    /// added one (and always for a scope that cannot receive one).
    fn extension_word(&self) -> Value {
        if self.has_extension_word() {
            // SAFETY: a context with the extension bit owns one more word
            // after its `slot_count` slots, initialized at allocation.
            unsafe { *self.slots_ptr().add(self.slot_count as usize) }
        } else {
            Value::undefined()
        }
    }

    /// Eval extension handle, if a sloppy direct eval has added one.
    pub(crate) fn extension_handle(&self) -> Option<EvalExtensionHandle> {
        self.extension_word().as_eval_extension()
    }

    /// Report the descriptor's owning function and every function id the
    /// context's values keep alive.
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        visitor(self.scope_function_id);
        crate::code_liveness::visit_value(&self.parent, visitor);
        crate::code_liveness::visit_value(&self.extension_word(), visitor);
        for value in self.slots() {
            crate::code_liveness::visit_value(value, visitor);
        }
    }
}

/// Scope identity and shape of a context about to be allocated.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ContextShape {
    /// Function whose scope table describes the context.
    pub(crate) scope_function_id: u32,
    /// Index into that table.
    pub(crate) scope_index: u16,
    /// Number of slots.
    pub(crate) slot_count: u16,
    /// The scope's descriptor has `has_extension`: the cell reserves the
    /// eval-extension word after its slots.
    pub(crate) has_extension: bool,
}

impl ContextShape {
    fn scope_index_word(self) -> u16 {
        debug_assert!(self.scope_index & CONTEXT_HAS_EXTENSION == 0);
        if self.has_extension {
            self.scope_index | CONTEXT_HAS_EXTENSION
        } else {
            self.scope_index
        }
    }

    fn trailing_bytes(self) -> usize {
        (self.slot_count as usize + usize::from(self.has_extension)) * std::mem::size_of::<Value>()
    }
}

/// Allocate a context of `shape` under `parent`, starting slot `i` as the
/// hole when `initial_hole(i)` and as `undefined` otherwise.
///
/// `parent` rides through the allocation in the pending body, so a
/// collection triggered here rewrites it; callers holding other young values
/// in Rust locals expose them through `external_visit`.
///
/// # Errors
/// Surfaces [`OutOfMemory`] verbatim.
pub(crate) fn alloc_context_with_roots(
    heap: &mut GcHeap,
    shape: ContextShape,
    parent: Value,
    initial_hole: impl Fn(usize) -> bool,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ContextHandle, OutOfMemory> {
    debug_assert!(parent.is_undefined() || parent.as_context().is_some());
    let body = ContextBody {
        scope_function_id: shape.scope_function_id,
        scope_index: shape.scope_index_word(),
        slot_count: shape.slot_count,
        parent,
    };
    heap.alloc_trailing_with_roots_initialized(
        body,
        shape.trailing_bytes(),
        external_visit,
        |body| {
            for (index, slot) in body.slots_mut().iter_mut().enumerate() {
                *slot = if initial_hole(index) {
                    Value::hole()
                } else {
                    Value::undefined()
                };
            }
            body.initialize_extension_word();
        },
    )
}

/// §14.7.4.4 CreatePerIterationEnvironment: allocate a copy of the context
/// held at `*source` — same scope identity, parent, and slot values (holes
/// included).
///
/// `source` must point at a traced register (or another slot the heap's root
/// providers or `external_visit` rewrite in place). The copy reads the source
/// only through that slot after the allocation, so a collection the
/// allocation triggers cannot leave it reading a moved body.
///
/// # Errors
/// Surfaces [`OutOfMemory`] verbatim.
///
/// # Safety
/// `source` must stay valid and hold a context value for the whole call.
pub(crate) unsafe fn copy_context_with_roots(
    heap: &mut GcHeap,
    source: *const Value,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<ContextHandle, OutOfMemory> {
    // SAFETY: caller contract.
    let handle = unsafe { *source }
        .as_context()
        .expect("CopyContext source holds a context");
    let (shape, parent) = heap.read_payload(handle, |body| {
        debug_assert!(
            body.extension_word().is_undefined(),
            "a scope with an eval extension is never copied"
        );
        (
            ContextShape {
                scope_function_id: body.scope_function_id,
                scope_index: body.scope_index(),
                slot_count: body.slot_count,
                has_extension: body.has_extension_word(),
            },
            body.parent,
        )
    });
    let body = ContextBody {
        scope_function_id: shape.scope_function_id,
        scope_index: shape.scope_index_word(),
        slot_count: shape.slot_count,
        parent,
    };
    heap.alloc_trailing_with_roots_initialized(
        body,
        shape.trailing_bytes(),
        external_visit,
        |body| {
            // SAFETY: the source slot is traced, so it names the source's
            // current address after any collection this allocation ran.
            let current = unsafe { *source }
                .as_context()
                .expect("CopyContext source still holds a context");
            let payload = context_payload_ptr(current);
            // SAFETY: `current` is a live context with the same slot count;
            // the new cell is disjoint from it.
            let from = unsafe { &*payload };
            body.slots_mut().copy_from_slice(from.slots());
            body.initialize_extension_word();
        },
    )
}

fn context_payload_ptr(handle: ContextHandle) -> *const ContextBody {
    // SAFETY: a context handle names a header immediately followed by its
    // payload in the same cell.
    unsafe {
        handle
            .as_header_ptr()
            .cast::<u8>()
            .add(otter_gc::header::HEADER_SIZE)
            .cast::<ContextBody>()
    }
}

/// `(scope_function_id, scope_index)` of `context`.
#[must_use]
pub(crate) fn scope_identity(heap: &GcHeap, context: ContextHandle) -> (u32, u16) {
    heap.read_payload(context, |body| (body.scope_function_id, body.scope_index()))
}

/// Parent of `context`, `None` for the outermost.
#[must_use]
pub(crate) fn parent(heap: &GcHeap, context: ContextHandle) -> Option<ContextHandle> {
    heap.read_payload(context, ContextBody::parent_handle)
}

/// Follow `depth` parent links from `context`. `None` when the chain is
/// shorter, which verified bytecode never produces.
#[must_use]
pub(crate) fn walk(heap: &GcHeap, context: ContextHandle, depth: u16) -> Option<ContextHandle> {
    let mut current = context;
    for _ in 0..depth {
        current = parent(heap, current)?;
    }
    Some(current)
}

/// Slot `slot` of `context`.
#[must_use]
pub(crate) fn read_slot(heap: &GcHeap, context: ContextHandle, slot: u16) -> Option<Value> {
    heap.read_payload(context, |body| body.slots().get(slot as usize).copied())
}

/// Store `value` into slot `slot` of `context` and record the write barrier.
/// Returns `false` for an out-of-range slot.
pub(crate) fn write_slot(
    heap: &mut GcHeap,
    context: ContextHandle,
    slot: u16,
    value: Value,
) -> bool {
    let stored = heap.with_payload(context, |body| {
        match body.slots_mut().get_mut(slot as usize) {
            Some(target) => {
                *target = value;
                true
            }
            None => false,
        }
    });
    if stored {
        heap.record_write(context, &value);
    }
    stored
}

/// The eval extension of `context`, if any.
#[must_use]
pub(crate) fn extension(heap: &GcHeap, context: ContextHandle) -> Option<EvalExtensionHandle> {
    heap.read_payload(context, ContextBody::extension_handle)
}

/// Install `extension` on `context` and record the write barrier.
pub(crate) fn set_extension(
    heap: &mut GcHeap,
    context: ContextHandle,
    extension: EvalExtensionHandle,
) {
    let value = Value::eval_extension(extension);
    heap.with_payload(context, |body| {
        assert!(
            body.has_extension_word(),
            "an eval extension is installed only on a scope that reserves it"
        );
        // SAFETY: the cell owns the extension word after its slots.
        unsafe { *body.slots_ptr().add(body.slot_count as usize) = value };
    });
    heap.record_write(context, &value);
}

/// First eval extension holding `name` among the contexts at hops
/// `[0, depth)` from `context`, innermost first.
///
/// Only a scope whose descriptor sets `has_extension` ever carries an
/// extension, so every non-`undefined` extension field on the walk is an
/// extension-capable scope.
#[must_use]
pub(crate) fn probe_extensions(
    heap: &GcHeap,
    context: Option<ContextHandle>,
    depth: u16,
    name: &str,
) -> Option<EvalExtensionHandle> {
    let mut current = context;
    for _ in 0..depth {
        let handle = current?;
        let (extension, parent) = heap.read_payload(handle, |body| {
            (body.extension_handle(), body.parent_handle())
        });
        if let Some(extension) = extension
            && crate::eval_env::extension_has(heap, extension, name)
        {
            return Some(extension);
        }
        current = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_gc::raw::RawGc;

    fn shape(slot_count: u16) -> ContextShape {
        ContextShape {
            scope_function_id: 3,
            scope_index: 1,
            slot_count,
            has_extension: true,
        }
    }

    #[test]
    fn layout_matches_the_published_offsets() {
        assert_eq!(std::mem::size_of::<ContextBody>(), 16);
        assert_eq!(CONTEXT_BODY_PARENT_OFFSET, 8);
        assert_eq!(CONTEXT_BODY_SLOTS_OFFSET, 16);
        assert_eq!(
            <ContextBody as otter_gc::SafeTraceable>::TYPE_TAG,
            CONTEXT_BODY_TYPE_TAG
        );
    }

    #[test]
    fn a_fresh_context_starts_each_slot_per_its_kind() {
        let mut heap = GcHeap::new().expect("heap");
        let context = alloc_context_with_roots(
            &mut heap,
            shape(3),
            Value::undefined(),
            |index| index == 1,
            &mut |_| {},
        )
        .expect("context");
        assert_eq!(read_slot(&heap, context, 0), Some(Value::undefined()));
        assert_eq!(read_slot(&heap, context, 1), Some(Value::hole()));
        assert_eq!(read_slot(&heap, context, 2), Some(Value::undefined()));
        assert_eq!(read_slot(&heap, context, 3), None);
        assert_eq!(scope_identity(&heap, context), (3, 1));
        assert_eq!(parent(&heap, context), None);
        assert_eq!(extension(&heap, context), None);
    }

    #[test]
    fn a_young_context_chain_survives_forced_scavenges_and_a_full_collection() {
        let mut heap = GcHeap::new().expect("heap");
        let outer = alloc_context_with_roots(
            &mut heap,
            shape(1),
            Value::undefined(),
            |_| false,
            &mut |_| {},
        )
        .expect("outer");
        let payload = crate::object::alloc_fixture_object_with_roots(&mut heap, &mut |_| {})
            .expect("young payload");
        assert!(write_slot(&mut heap, outer, 0, Value::object(payload)));
        let mut outer_value = Value::context(outer);
        let inner = {
            let outer_slot: *mut Value = &mut outer_value;
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                // SAFETY: `outer_value` lives across the allocation.
                unsafe { (*outer_slot).trace_value_slot_mut(visitor) };
            };
            alloc_context_with_roots(&mut heap, shape(2), outer_value, |_| true, &mut roots)
                .expect("inner")
        };
        let mut inner_value = Value::context(inner);
        let before = inner_value.to_bits();
        for round in 0..3 {
            let slot: *mut Value = &mut inner_value;
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                // SAFETY: `inner_value` lives across the collection.
                unsafe { (*slot).trace_value_slot_mut(visitor) };
            };
            if round == 2 {
                heap.collect_full(&mut roots).expect("full");
            } else {
                heap.collect_minor_with_roots(&mut roots).expect("scavenge");
            }
        }
        assert_ne!(inner_value.to_bits(), before, "the young context moved");
        let inner = inner_value.as_context().expect("still a context");
        assert_eq!(read_slot(&heap, inner, 0), Some(Value::hole()));
        let outer = walk(&heap, inner, 1).expect("parent survives");
        let held = read_slot(&heap, outer, 0)
            .and_then(Value::as_object)
            .expect("outer slot still names the object");
        assert_eq!(
            heap.debug_header_tag(held),
            Some(crate::object::OBJECT_BODY_TYPE_TAG)
        );
        assert_eq!(walk(&heap, inner, 2), None);
    }

    #[test]
    fn copy_context_duplicates_slots_holes_and_parent() {
        let mut heap = GcHeap::new().expect("heap");
        let parent_ctx = alloc_context_with_roots(
            &mut heap,
            shape(0),
            Value::undefined(),
            |_| false,
            &mut |_| {},
        )
        .expect("parent");
        let source = alloc_context_with_roots(
            &mut heap,
            shape(2),
            Value::context(parent_ctx),
            |index| index == 1,
            &mut |_| {},
        )
        .expect("source");
        assert!(write_slot(&mut heap, source, 0, Value::number_i32(41)));
        let register = Value::context(source);
        let copy = unsafe {
            copy_context_with_roots(&mut heap, std::ptr::from_ref(&register), &mut |_| {})
        }
        .expect("copy");
        assert_ne!(copy, source);
        assert_eq!(read_slot(&heap, copy, 0), Some(Value::number_i32(41)));
        assert_eq!(read_slot(&heap, copy, 1), Some(Value::hole()));
        assert_eq!(parent(&heap, copy), Some(parent_ctx));
        assert_eq!(scope_identity(&heap, copy), scope_identity(&heap, source));
        // The copy is independent: writing it leaves the source intact.
        assert!(write_slot(&mut heap, copy, 0, Value::number_i32(42)));
        assert_eq!(read_slot(&heap, source, 0), Some(Value::number_i32(41)));
    }

    #[test]
    fn contexts_are_an_internal_value_family() {
        let mut heap = GcHeap::new().expect("heap");
        let context = alloc_context_with_roots(
            &mut heap,
            shape(1),
            Value::undefined(),
            |_| false,
            &mut |_| {},
        )
        .expect("context");
        let value = Value::context(context);
        assert!(!value.is_object_type());
        assert!(!value.is_object_like());
        assert!(!value.is_callable());
        assert!(!value.is_primitive());
        assert_eq!(value.kind(), crate::ValueKind::Internal);
        assert_eq!(value.as_context(), Some(context));
        assert_eq!(value.as_eval_extension(), None);
    }
}
