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

/// Function-id ranges of the chunks one reclamation pass evicted, sorted.
#[derive(Debug, Default)]
pub(crate) struct EvictedRanges {
    ranges: Vec<(u32, u32)>,
}

impl EvictedRanges {
    pub(crate) fn new(evicted: &[crate::code_space::ChunkEvictionCandidate]) -> Self {
        let mut ranges: Vec<(u32, u32)> = evicted
            .iter()
            .map(|candidate| (candidate.function_base, candidate.function_end()))
            .collect();
        ranges.sort_unstable();
        Self { ranges }
    }

    /// Whether `function_id` belongs to an evicted chunk.
    pub(crate) fn contains(&self, function_id: u32) -> bool {
        let after = self
            .ranges
            .partition_point(|&(start, _)| start <= function_id);
        after
            .checked_sub(1)
            .is_some_and(|index| function_id < self.ranges[index].1)
    }
}

fn prune_chain(
    heap: &mut GcHeap,
    head: ConstructorLayout,
    evicted: &EvictedRanges,
) -> ConstructorLayout {
    let mut current = head;
    let mut retained_head = ConstructorLayout::null();
    let mut last = ConstructorLayout::null();
    while !current.is_null() {
        let (base, next) = heap.read_payload(current, |body| (body.base_function_id, body.next));
        if !evicted.contains(base) {
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

/// The handle of a live `T` body the heap walk handed out.
fn handle_of<T: Traceable>(body: &T) -> Gc<T> {
    let header = std::ptr::from_ref(body) as usize - otter_gc::header::HEADER_SIZE;
    let offset = (header - otter_gc::cage_base() as usize) as u32;
    // SAFETY: the typed heap walk proved this live T cell. No GC occurs
    // between gathering handles and the completed owner mutation pass.
    unsafe { Gc::from_offset(offset) }
}

/// Every live owner of a constructor-layout chain, gathered in one walk of
/// the heap.
#[derive(Default)]
struct LayoutOwners {
    closures: Vec<Gc<crate::closure_construct::ClosureRareBody>>,
    classes: Vec<Gc<crate::class_constructor::ClassConstructorBody>>,
    bound: Vec<Gc<crate::bound_function::BoundFunctionBody>>,
    proxies: Vec<Gc<crate::proxy::ProxyBodyGc>>,
    natives: Vec<Gc<crate::native_function::NativeFunctionBody>>,
}

impl LayoutOwners {
    fn gather(heap: &GcHeap) -> Self {
        let mut owners = Self::default();
        heap.for_each_live_body(|_, body| {
            if let Some(body) = body.downcast::<crate::closure_construct::ClosureRareBody>() {
                owners.closures.push(handle_of(body));
            } else if let Some(body) =
                body.downcast::<crate::class_constructor::ClassConstructorBody>()
            {
                owners.classes.push(handle_of(body));
            } else if let Some(body) = body.downcast::<crate::bound_function::BoundFunctionBody>() {
                owners.bound.push(handle_of(body));
            } else if let Some(body) = body.downcast::<crate::proxy::ProxyBodyGc>() {
                owners.proxies.push(handle_of(body));
            } else if let Some(body) = body.downcast::<crate::native_function::NativeFunctionBody>()
            {
                owners.natives.push(handle_of(body));
            }
        });
        owners
    }
}

impl crate::Interpreter {
    /// Unlink the constructor layouts keyed by functions of `evicted`
    /// chunks from every owner, walking the heap once for all of them.
    pub(crate) fn prune_constructor_layouts_for_evicted(&mut self, evicted: &EvictedRanges) {
        debug_assert!(!self.jit_has_native_frames());
        let owners = LayoutOwners::gather(&self.gc_heap);
        for owner in owners.closures {
            let head = self
                .gc_heap
                .read_payload(owner, |body| body.constructor_layouts);
            let head = prune_chain(&mut self.gc_heap, head, evicted);
            self.gc_heap
                .with_payload(owner, |body| body.constructor_layouts = head);
            self.gc_heap.record_write(owner, &head);
        }
        for owner in owners.classes {
            let owner = crate::class_constructor::ClassConstructor::from_gc(owner);
            let head = owner.constructor_layouts(&self.gc_heap);
            let head = prune_chain(&mut self.gc_heap, head, evicted);
            owner.set_constructor_layouts(&mut self.gc_heap, head);
        }
        for owner in owners.bound {
            let owner = crate::bound_function::BoundFunction::from_gc(owner);
            let head = owner.constructor_layouts(&self.gc_heap);
            let head = prune_chain(&mut self.gc_heap, head, evicted);
            owner.set_constructor_layouts(&mut self.gc_heap, head);
        }
        for owner in owners.proxies {
            let owner = crate::proxy::JsProxy::from_handle(owner);
            let head = owner.constructor_layouts(&self.gc_heap);
            let head = prune_chain(&mut self.gc_heap, head, evicted);
            owner.set_constructor_layouts(&mut self.gc_heap, head);
        }
        for owner in owners.natives {
            let owner = crate::native_function::NativeFunction::from_gc(owner);
            let head = owner.constructor_layouts(&self.gc_heap);
            let head = prune_chain(&mut self.gc_heap, head, evicted);
            owner.set_constructor_layouts(&mut self.gc_heap, head);
        }
        self.function_constructor_layouts
            .retain(|target, _| !evicted.contains(*target));
        // This registry is a rooted owner table for immediate callable values;
        // its keys never enter the code-ID census as semantic function values.
        let targets = self
            .function_constructor_layouts
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for target in targets {
            let head = self.function_constructor_layouts[&target];
            let head = prune_chain(&mut self.gc_heap, head, evicted);
            if head.is_null() {
                self.function_constructor_layouts.remove(&target);
            } else {
                self.function_constructor_layouts.insert(target, head);
            }
        }
    }
}
