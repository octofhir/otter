//! Cold removal of evicted base-function selection keys.
//!
//! # Contents
//! - Bounded family-list unlinking without GC or JavaScript work.
//! - Exact existing GC owner heads and the immediate-function registry.
//!
//! # Invariants
//! The code-eviction owner has completed its full-GC census and proved no
//! materialized/native activation is live. A family's base id is metadata,
//! never a semantic callable root. Only genuinely unreachable evicted ranges
//! reach this hook; ids are never reused. Existing prototype/field shapes own
//! their real callable values and are covered by the normal heap census.
//! This hook allocates only Rust scratch vectors; the GC heap cannot move
//! during the walk, and no layout, shape or receiver is repointed.
//!
//! # See also
//! - `crate::code_liveness` distinguishes semantic values from selection keys.
//! - `crate::interp::exec` invokes this only after successful code eviction.

use super::ConstructorLayout;
use otter_gc::{Gc, GcHeap, Traceable};

fn prune_chain(
    heap: &mut GcHeap,
    head: ConstructorLayout,
    start: u32,
    end: u32,
) -> ConstructorLayout {
    let mut current = head;
    let mut retained_head = ConstructorLayout::null();
    let mut last = ConstructorLayout::null();
    while !current.is_null() {
        let (base, next) = heap.read_payload(current, |body| (body.base_function_id, body.next));
        if base < start || base >= end {
            if retained_head.is_null() {
                retained_head = current;
            }
            if !last.is_null() {
                heap.with_payload(last, |body| body.next = current);
                heap.record_write(last, &current);
            }
            last = current;
        } else {
            heap.with_payload(current, |body| body.detached = true);
        }
        current = next;
    }
    if !last.is_null() {
        heap.with_payload(last, |body| body.next = ConstructorLayout::null());
    }
    retained_head
}

/// Recover existing live engine handles from the authoritative heap payload
/// walk. No handle or raw pointer crosses an external/contributor boundary.
fn owner_handles<T: Traceable>(heap: &GcHeap) -> Vec<Gc<T>> {
    let mut handles = Vec::new();
    heap.for_each_live_payload::<T, _>(|_, body| {
        let header = body as *const T as usize - otter_gc::header::HEADER_SIZE;
        let offset = (header - otter_gc::cage_base() as usize) as u32;
        // SAFETY: the typed heap walk proved this live T cell. No GC occurs
        // between gathering handles and the completed owner mutation pass.
        handles.push(unsafe { Gc::from_offset(offset) });
    });
    handles
}

impl crate::Interpreter {
    pub(crate) fn prune_constructor_layouts_for_evicted_range(&mut self, start: u32, end: u32) {
        debug_assert!(!self.jit_has_native_frames());
        for owner in owner_handles::<crate::closure_construct::ClosureRareBody>(&self.gc_heap) {
            let head = self
                .gc_heap
                .read_payload(owner, |body| body.constructor_layouts);
            let head = prune_chain(&mut self.gc_heap, head, start, end);
            self.gc_heap
                .with_payload(owner, |body| body.constructor_layouts = head);
            self.gc_heap.record_write(owner, &head);
        }
        for owner in owner_handles::<crate::class_constructor::ClassConstructorBody>(&self.gc_heap)
        {
            let owner = crate::class_constructor::ClassConstructor::from_gc(owner);
            let head = owner.constructor_layouts(&self.gc_heap);
            let head = prune_chain(&mut self.gc_heap, head, start, end);
            owner.set_constructor_layouts(&mut self.gc_heap, head);
        }
        for owner in owner_handles::<crate::bound_function::BoundFunctionBody>(&self.gc_heap) {
            let owner = crate::bound_function::BoundFunction::from_gc(owner);
            let head = owner.constructor_layouts(&self.gc_heap);
            let head = prune_chain(&mut self.gc_heap, head, start, end);
            owner.set_constructor_layouts(&mut self.gc_heap, head);
        }
        for owner in owner_handles::<crate::proxy::ProxyBodyGc>(&self.gc_heap) {
            let owner = crate::proxy::JsProxy::from_handle(owner);
            let head = owner.constructor_layouts(&self.gc_heap);
            let head = prune_chain(&mut self.gc_heap, head, start, end);
            owner.set_constructor_layouts(&mut self.gc_heap, head);
        }
        for owner in owner_handles::<crate::native_function::NativeFunctionBody>(&self.gc_heap) {
            let owner = crate::native_function::NativeFunction::from_gc(owner);
            let head = owner.constructor_layouts(&self.gc_heap);
            let head = prune_chain(&mut self.gc_heap, head, start, end);
            owner.set_constructor_layouts(&mut self.gc_heap, head);
        }
        self.function_constructor_layouts
            .retain(|target, _| *target < start || *target >= end);
        // This registry is a rooted owner table for immediate callable values;
        // its keys never enter the code-ID census as semantic function values.
        let targets = self
            .function_constructor_layouts
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for target in targets {
            let head = self.function_constructor_layouts[&target];
            let head = prune_chain(&mut self.gc_heap, head, start, end);
            if head.is_null() {
                self.function_constructor_layouts.remove(&target);
            } else {
                self.function_constructor_layouts.insert(target, head);
            }
        }
    }
}
