//! Integration tests for `#[derive(Groom)]`.
//!
//! Each test exercises one shape supported by the derive:
//!
//! - named struct whose finalize work tracks a host counter,
//! - tuple struct with a `#[groom(via = path)]` per-field hook,
//! - body whose only fields are `#[groom(skip)]` primitives, so the
//!   generated finalize is empty.
//!
//! The sweep-time wiring is exercised by the GC integration test at
//! `crates/otter-gc/tests/sweep_finalize.rs`; these tests pin the
//! macro expansion shape.

use std::sync::atomic::{AtomicU32, Ordering};

use otter_gc::raw::SlotVisitor;
use otter_gc::{SafeFinalize, SafeTraceable};
use otter_macros::{Groom, Pelt};

const GROOM_NAMED_TAG: u8 = 0xD1;
const GROOM_TUPLE_TAG: u8 = 0xD2;
const GROOM_EMPTY_TAG: u8 = 0xD3;

static GROOM_COUNTER: AtomicU32 = AtomicU32::new(0);

fn bump_counter() {
    GROOM_COUNTER.fetch_add(1, Ordering::Relaxed);
}

/// Custom `GroomField`-compatible cell that increments the global
/// counter when its `groom` method fires.
#[derive(Default)]
struct BumpField;

impl otter_vm::groom::GroomField for BumpField {
    fn groom(&mut self) {
        bump_counter();
    }
}

/// Per-field `via` hook used by [`TupleBody`].
fn bump_via(_field: &mut u32) {
    bump_counter();
}

#[derive(Pelt, Groom, Default)]
#[pelt(tag = GROOM_NAMED_TAG)]
struct NamedBody {
    /// Field with a real `GroomField` impl. Skip from Pelt because
    /// `BumpField` carries no GC slots and never implements
    /// `PeltField` — only `GroomField`.
    #[pelt(skip)]
    bumper: BumpField,
    /// Skipped primitive — derive should not call `GroomField::groom`
    /// here even if the type implements it.
    #[pelt(skip)]
    #[groom(skip)]
    #[allow(dead_code)]
    skipped_counter: u64,
}

#[derive(Pelt, Groom)]
#[pelt(tag = GROOM_TUPLE_TAG)]
struct TupleBody(
    #[groom(via = bump_via)]
    #[pelt(skip)]
    u32,
);

impl TupleBody {
    fn new(v: u32) -> Self {
        Self(v)
    }
}

#[derive(Pelt, Groom)]
#[pelt(tag = GROOM_EMPTY_TAG)]
struct EmptyBody {
    #[pelt(skip)]
    #[groom(skip)]
    #[allow(dead_code)]
    only: u32,
}

#[test]
fn derive_emits_safe_finalize_with_groom_field_calls() {
    GROOM_COUNTER.store(0, Ordering::Relaxed);
    let mut body = NamedBody::default();
    body.finalize_safe();
    assert_eq!(GROOM_COUNTER.load(Ordering::Relaxed), 1);
}

#[test]
fn tuple_field_via_hook_runs() {
    GROOM_COUNTER.store(0, Ordering::Relaxed);
    let mut body = TupleBody::new(7);
    body.finalize_safe();
    assert_eq!(GROOM_COUNTER.load(Ordering::Relaxed), 1);
}

#[test]
fn empty_body_finalize_compiles_and_is_a_noop() {
    GROOM_COUNTER.store(0, Ordering::Relaxed);
    let mut body = EmptyBody { only: 99 };
    body.finalize_safe();
    assert_eq!(GROOM_COUNTER.load(Ordering::Relaxed), 0);
}

#[test]
fn derive_satisfies_both_traceable_and_finalize() {
    // Compile-time assertion: every Groom body must also be a
    // SafeTraceable. The Rust trait bound enforces this; the test
    // pins the contract.
    fn assert_pelt_groom_pair<T: SafeTraceable + SafeFinalize>() {}
    assert_pelt_groom_pair::<NamedBody>();
    assert_pelt_groom_pair::<TupleBody>();
    assert_pelt_groom_pair::<EmptyBody>();
    let _ = std::mem::size_of::<&mut SlotVisitor<'_>>();
}
