//! Closure context birth, weak relocation and retirement proofs.
//!
//! # Contents
//! - Duplicate births, permanent widening, unsubscribe and moving observations.
//!
//! # Invariants
//! - A separate strong owner keeps a real context alive across a scavenge.
//! - The speculative fact cannot keep the context alive after that owner drops.
//!
//! # See also
//! - [`super::ClosureContexts`] owns the production source facts.

use super::*;

fn context(heap: &mut otter_gc::GcHeap) -> Value {
    Value::context(
        crate::context::alloc_context_with_roots(
            heap,
            crate::context::ContextShape {
                scope_function_id: 7,
                scope_index: 0,
                slot_count: 1,
                has_extension: false,
            },
            Value::undefined(),
            |_| false,
            &mut |_| {},
        )
        .expect("context"),
    )
}

#[test]
fn first_context_follows_its_live_owner_and_dies_without_one() {
    use std::cell::Cell;

    struct Source<'a> {
        contexts: &'a ClosureContexts,
        live: Cell<Value>,
    }
    impl otter_gc::ExtraRootSource for Source<'_> {
        fn visit_extra_roots(
            &self,
            _: otter_gc::RootSet,
            visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc),
        ) {
            let mut live = self.live.get();
            live.trace_value_slot_mut(visitor);
            self.live.set(live);
        }

        fn process_weak(
            &self,
            visitor: &mut dyn FnMut(otter_gc::raw::RawGc) -> Option<otter_gc::raw::RawGc>,
        ) {
            self.contexts.process_weak(visitor);
        }
    }

    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let first = context(&mut heap);
    let mut contexts = ClosureContexts::default();
    contexts.facts.insert(
        7,
        Some(Box::new(ContextFact {
            context: AtomicU64::new(first.to_bits()),
            subscribers: Vec::new(),
        })),
    );
    let address = contexts.single(7).unwrap().0;
    let retired = DependencyFlag::new();
    contexts.subscribe(7, &retired);
    contexts.unsubscribe(7, &retired);
    let active = DependencyFlag::new();
    contexts.subscribe(7, &active);
    let source = Source {
        contexts: &contexts,
        live: Cell::new(first),
    };
    let roots = heap.register_extra_roots(otter_gc::ExtraRoots::new(&source));
    for _ in 0..2 {
        heap.collect_minor_with_roots(&mut |_| {})
            .expect("scavenge");
    }
    let (moved_address, moved) = contexts.single(7).unwrap();
    assert_eq!(address, moved_address);
    assert_ne!(first.to_bits(), moved.to_bits());
    assert_eq!(source.live.get(), moved);
    assert!(moved.as_context().is_some());
    assert!(!active.is_set());
    source.live.set(Value::undefined());
    heap.collect_full(&mut |_| {}).expect("full collection");
    assert!(active.is_set());
    assert!(!retired.is_set());
    assert!(contexts.single(7).is_none());
    drop(roots);
    let other = context(&mut heap);
    contexts.observe(7, other);
    assert!(contexts.single(7).is_none(), "death is permanent widening");
}

#[test]
fn a_different_context_fires_once_and_permanently_releases_the_first() {
    let mut fact = ContextFact {
        context: AtomicU64::new(ContextFact::UNSEEN),
        subscribers: Vec::new(),
    };
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let first = context(&mut heap);
    let second = context(&mut heap);
    fact.observe(first);
    assert_eq!(fact.single(), Some(first));
    let flag = DependencyFlag::new();
    fact.subscribers.push(Arc::clone(&flag));
    fact.observe(first);
    assert!(!flag.is_set());
    fact.observe(second);
    assert!(flag.is_set());
    assert!(fact.single().is_none());
    assert!(fact.subscribers.is_empty());
    fact.observe(first);
    assert!(fact.single().is_none());
}
