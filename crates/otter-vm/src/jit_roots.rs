//! Lifetimes of heap assumptions retained by compiled code.
//!
//! # Contents
//! - [`CompilationRoot`] owns embedded proof allocations and identity cells.
//! - [`CalleeIdentityCell`] caches the callee a generated call site proved.
//! - [`InstanceofCell`] caches the target an `instanceof` site proved, with
//!   the prototype OrdinaryHasInstance searches for.
//! - Weak processing relocates live cache entries and forgets dead ones.
//!
//! # Invariants
//! - Compile sessions retain every embedded allocation until installation;
//!   an observation's payload remains weak even while its cell is retained.
//! - Installed generations retain roots through invalidation until physical
//!   retirement, including while an active frame can return into their code.
//! - Shapes are pinned GC cells; validity cells are address-stable Rust
//!   allocations containing no GC references.
//! - Writable callee and `instanceof` caches never own their observed values.
//!   Minor and full weak processing rewrite surviving handles before reuse
//!   or clear a dead entry before its offset can name another allocation.
//! - A held identity is an embedded constant, not a writable cache: the same
//!   carrier traces that value strongly for the generation's lifetime.
//! - The VM also empties an `instanceof` cache before a closure that ever
//!   filled one changes its `prototype`, own properties or `[[Prototype]]`
//!   (see `Interpreter::retire_instanceof_proofs_for`).
//!
//! # See also
//! - [`crate::jit_registry`] for installed generations.
//! - [`crate::object::prototype_validity`] for mutation dependencies.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{ShapeHandle, prototype_validity::PrototypeValidity};
use otter_gc::raw::RawGc;

/// The last callee value one generated call site proved to be its baked
/// target. A later call with the identical value skips the full proof; any
/// other value takes the proof and, on success, replaces the cached one.
#[derive(Debug)]
pub(crate) struct CalleeIdentityCell {
    value: AtomicU64,
    strong: bool,
}

impl CalleeIdentityCell {
    /// Bits no `Value` encoding produces: an empty cell matches nothing.
    pub(crate) const EMPTY: u64 = u64::MAX;

    pub(crate) fn new() -> Self {
        Self {
            value: AtomicU64::new(Self::EMPTY),
            strong: false,
        }
    }

    /// A strong constant cell, which generated code reads or compares.
    pub(crate) fn holding(value: crate::Value) -> Self {
        Self {
            value: AtomicU64::new(value.to_bits()),
            strong: true,
        }
    }

    /// Address generated code reads and writes.
    pub(crate) fn address(&self) -> u64 {
        self.value.as_ptr() as u64
    }

    /// Empty the cell when it holds a value `release` names. A call through
    /// an empty cell misses and takes the full proof, as for any other
    /// callee.
    fn release_if(&self, release: &mut dyn FnMut(crate::Value) -> bool) -> bool {
        let bits = self.value.load(Ordering::Relaxed);
        if bits == Self::EMPTY || !release(crate::Value::from_bits(bits)) {
            return false;
        }
        self.value.store(Self::EMPTY, Ordering::Relaxed);
        true
    }

    fn trace(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        if self.strong && crate::value::tag::is_cell_bits(self.value.load(Ordering::Relaxed)) {
            visitor(self.value.as_ptr().cast());
        }
    }

    fn process_weak(&self, visitor: &mut dyn FnMut(RawGc) -> Option<RawGc>) {
        if !self.strong {
            let bits = self.value.load(Ordering::Relaxed);
            self.value.store(
                rewritten_weak_bits(bits, visitor).unwrap_or(Self::EMPTY),
                Ordering::Relaxed,
            );
        }
    }
}

/// Keep a Value's immediate representation or rewrite only its cage offset.
fn rewritten_weak_bits(bits: u64, visitor: &mut dyn FnMut(RawGc) -> Option<RawGc>) -> Option<u64> {
    if !crate::value::tag::is_cell_bits(bits) {
        return Some(bits);
    }
    visitor(RawGc(bits as u32)).map(|raw| (bits & !u64::from(u32::MAX)) | u64::from(raw.0))
}

/// The last `instanceof` target one generated site proved to answer through
/// the default `@@hasInstance`: an ordinary closure without own properties
/// or a `[[Prototype]]` override, whose `prototype` held an ordinary object,
/// with that object. A later test against the identical target reads the
/// cached prototype; the VM empties the cell before any change that could
/// alter that answer becomes observable.
#[derive(Debug)]
#[repr(C)]
pub(crate) struct InstanceofCell {
    target: AtomicU64,
    prototype: AtomicU64,
}

const _: () = assert!(
    std::mem::offset_of!(InstanceofCell, target)
        == crate::jit::JIT_INSTANCEOF_CELL_TARGET_OFFSET as usize
);
const _: () = assert!(
    std::mem::offset_of!(InstanceofCell, prototype)
        == crate::jit::JIT_INSTANCEOF_CELL_PROTOTYPE_OFFSET as usize
);

impl InstanceofCell {
    pub(crate) fn new() -> Self {
        Self {
            target: AtomicU64::new(CalleeIdentityCell::EMPTY),
            prototype: AtomicU64::new(CalleeIdentityCell::EMPTY),
        }
    }

    /// Address generated code reads and writes.
    pub(crate) fn address(&self) -> u64 {
        std::ptr::from_ref(self) as u64
    }

    /// Forget the cached pair: the next test re-proves its target.
    pub(crate) fn clear(&self) {
        self.target
            .store(CalleeIdentityCell::EMPTY, Ordering::Relaxed);
        self.prototype
            .store(CalleeIdentityCell::EMPTY, Ordering::Relaxed);
    }

    fn process_weak(&self, visitor: &mut dyn FnMut(RawGc) -> Option<RawGc>) {
        let target = rewritten_weak_bits(self.target.load(Ordering::Relaxed), visitor);
        let prototype = rewritten_weak_bits(self.prototype.load(Ordering::Relaxed), visitor);
        if let (Some(target), Some(prototype)) = (target, prototype) {
            self.target.store(target, Ordering::Relaxed);
            self.prototype.store(prototype, Ordering::Relaxed);
        } else {
            self.clear();
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CompilationRoot {
    Shape(ShapeHandle),
    /// A constructor family a receiver plan names by its cell handle.
    Family(crate::constructor_layout::ConstructorLayout),
    Prototype(Arc<PrototypeValidity>),
    CalleeIdentity(Arc<CalleeIdentityCell>),
    Instanceof(Arc<InstanceofCell>),
}

impl CompilationRoot {
    pub(crate) fn key(&self) -> (u8, usize) {
        match self {
            Self::Shape(shape) => (0, shape.offset() as usize),
            Self::Family(family) => (4, family.offset() as usize),
            Self::Prototype(cell) => (1, cell.address()),
            Self::CalleeIdentity(cell) => (2, cell.address() as usize),
            Self::Instanceof(cell) => (3, cell.address() as usize),
        }
    }

    /// Empty a callee identity cell holding a value `release` names.
    pub(crate) fn release_callee_if(&self, release: &mut dyn FnMut(crate::Value) -> bool) -> bool {
        match self {
            Self::CalleeIdentity(cell) => cell.release_if(release),
            Self::Shape(_) | Self::Family(_) | Self::Prototype(_) | Self::Instanceof(_) => false,
        }
    }

    pub(crate) fn trace(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        match self {
            // Shapes never move, so the tracer never rewrites this slot.
            Self::Shape(shape) => visitor(std::ptr::from_ref(shape).cast_mut().cast()),
            // Family cells live in old space and never move either.
            Self::Family(family) => visitor(std::ptr::from_ref(family).cast_mut().cast()),
            Self::Prototype(_) => {}
            Self::CalleeIdentity(cell) => cell.trace(visitor),
            Self::Instanceof(_) => {}
        }
    }

    /// Rewrite live observations and forget dead ones after strong tracing.
    pub(crate) fn process_weak(&self, visitor: &mut dyn FnMut(RawGc) -> Option<RawGc>) {
        match self {
            Self::CalleeIdentity(cell) => cell.process_weak(visitor),
            Self::Instanceof(cell) => cell.process_weak(visitor),
            Self::Shape(_) | Self::Family(_) | Self::Prototype(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Owners {
        roots: Vec<CompilationRoot>,
        live: Cell<[crate::Value; 2]>,
    }

    impl otter_gc::ExtraRootSource for Owners {
        fn visit_extra_roots(&self, _: otter_gc::RootSet, visitor: &mut dyn FnMut(*mut RawGc)) {
            let mut live = self.live.get();
            for value in &mut live {
                value.trace_value_slot_mut(visitor);
            }
            self.live.set(live);
            for root in &self.roots {
                root.trace(visitor);
            }
        }

        fn process_weak(&self, visitor: &mut dyn FnMut(RawGc) -> Option<RawGc>) {
            for root in &self.roots {
                root.process_weak(visitor);
            }
        }
    }

    fn context(heap: &mut otter_gc::GcHeap) -> crate::Value {
        crate::Value::context(
            crate::context::alloc_context_with_roots(
                heap,
                crate::context::ContextShape {
                    scope_function_id: 7,
                    scope_index: 0,
                    slot_count: 1,
                    has_extension: false,
                },
                crate::Value::undefined(),
                |_| false,
                &mut |_| {},
            )
            .expect("context"),
        )
    }

    #[test]
    fn writable_cache_tracks_real_owner_but_constant_remains_an_owner() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let observed = context(&mut heap);
        let constant = context(&mut heap);
        let cache = Arc::new(CalleeIdentityCell::new());
        cache.value.store(observed.to_bits(), Ordering::Relaxed);
        let held = Arc::new(CalleeIdentityCell::holding(constant));
        let owners = Owners {
            roots: vec![
                CompilationRoot::CalleeIdentity(Arc::clone(&cache)),
                CompilationRoot::CalleeIdentity(Arc::clone(&held)),
            ],
            live: Cell::new([observed, crate::Value::undefined()]),
        };
        let _registration = heap.register_extra_roots(otter_gc::ExtraRoots::new(&owners));
        heap.collect_minor_with_roots(&mut |_| {})
            .expect("moving collection");
        assert_ne!(owners.live.get()[0], observed);
        assert_eq!(
            cache.value.load(Ordering::Relaxed),
            owners.live.get()[0].to_bits()
        );
        assert_ne!(held.value.load(Ordering::Relaxed), constant.to_bits());
        owners.live.set([crate::Value::undefined(); 2]);
        heap.collect_full(&mut |_| {}).expect("full collection");
        assert_eq!(
            cache.value.load(Ordering::Relaxed),
            CalleeIdentityCell::EMPTY
        );
        let held_value = crate::Value::from_bits(held.value.load(Ordering::Relaxed));
        assert_eq!(
            heap.raw_type_tag(held_value.as_raw_gc().expect("constant")),
            Some(crate::context::CONTEXT_BODY_TYPE_TAG)
        );
    }

    #[test]
    fn instanceof_pair_moves_together_and_forgets_both_if_one_dies() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let target = context(&mut heap);
        let prototype = context(&mut heap);
        let pair = Arc::new(InstanceofCell::new());
        pair.target.store(target.to_bits(), Ordering::Relaxed);
        pair.prototype.store(prototype.to_bits(), Ordering::Relaxed);
        let owners = Owners {
            roots: vec![CompilationRoot::Instanceof(Arc::clone(&pair))],
            live: Cell::new([target, prototype]),
        };
        let _registration = heap.register_extra_roots(otter_gc::ExtraRoots::new(&owners));
        heap.collect_minor_with_roots(&mut |_| {})
            .expect("moving collection");
        assert_ne!(owners.live.get()[0], target);
        assert_ne!(owners.live.get()[1], prototype);
        assert_eq!(
            pair.target.load(Ordering::Relaxed),
            owners.live.get()[0].to_bits()
        );
        assert_eq!(
            pair.prototype.load(Ordering::Relaxed),
            owners.live.get()[1].to_bits()
        );
        owners
            .live
            .set([owners.live.get()[0], crate::Value::undefined()]);
        heap.collect_full(&mut |_| {})
            .expect("partial reachability");
        assert_eq!(
            pair.target.load(Ordering::Relaxed),
            CalleeIdentityCell::EMPTY
        );
        assert_eq!(
            pair.prototype.load(Ordering::Relaxed),
            CalleeIdentityCell::EMPTY
        );
    }
}
