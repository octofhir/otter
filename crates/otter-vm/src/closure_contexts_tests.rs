//! Closure context birth, retirement and moving-root proofs.
//!
//! # Contents
//! - Duplicate births, permanent widening, unsubscribe and moving roots.
//!
//! # Invariants
//! - Roots are real collector-managed contexts; only scalar bits survive a move.
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
fn first_context_is_a_rewritten_root_and_many_drops_it() {
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
    for _ in 0..2 {
        heap.collect_minor_with_roots(&mut |visitor| contexts.trace(visitor))
            .expect("scavenge");
    }
    let (moved_address, moved) = contexts.single(7).unwrap();
    assert_eq!(address, moved_address);
    assert_ne!(first.to_bits(), moved.to_bits());
    assert!(moved.as_context().is_some());
    let retired = DependencyFlag::new();
    contexts.subscribe(7, &retired);
    contexts.unsubscribe(7, &retired);
    let active = DependencyFlag::new();
    contexts.subscribe(7, &active);
    contexts.observe(7, moved);
    assert!(!active.is_set());
    let other = context(&mut heap);
    contexts.observe(7, other);
    assert!(active.is_set());
    assert!(!retired.is_set());
    let mut roots = 0;
    contexts.trace(&mut |_| roots += 1);
    assert_eq!(roots, 0);
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
