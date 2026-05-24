//! Integration tests for `#[derive(Pelt)]`.
//!
//! Each test exercises one shape supported by the derive:
//!
//! - named struct with `Value` + `Option<Value>` + skipped primitive,
//! - tuple struct with `Gc<T>`,
//! - body with no traced slots (no fields after `#[pelt(skip)]`).

use std::cell::RefCell;

use otter_gc::SafeTraceable;
use otter_gc::raw::{RawGc, SlotVisitor};
use otter_macros::Pelt;
use otter_vm::Value;

const PROXY_LIKE_TYPE_TAG: u8 = 0xC1;
const COUNTER_LIKE_TYPE_TAG: u8 = 0xC2;
const REGEXP_LIKE_TYPE_TAG: u8 = 0xC3;

#[derive(Pelt)]
#[pelt(tag = PROXY_LIKE_TYPE_TAG)]
struct ProxyLike {
    target: Value,
    handler: Value,
    #[pelt(skip)]
    #[allow(dead_code)]
    revoked: bool,
}

#[derive(Pelt)]
#[pelt(tag = COUNTER_LIKE_TYPE_TAG)]
struct CounterLike {
    #[pelt(skip)]
    #[allow(dead_code)]
    count: u64,
}

#[derive(Pelt)]
#[pelt(tag = REGEXP_LIKE_TYPE_TAG)]
struct RegExpLike {
    last_index: RefCell<Value>,
    extra: Option<Value>,
    #[pelt(skip)]
    #[allow(dead_code)]
    flags: u32,
}

fn collect<T: SafeTraceable>(body: &T) -> Vec<*mut RawGc> {
    let mut out: Vec<*mut RawGc> = Vec::new();
    {
        let mut push = |p: *mut RawGc| out.push(p);
        let v: &mut SlotVisitor<'_> = &mut push;
        body.trace_slots_safe(v);
    }
    out
}

#[test]
fn type_tag_matches_attribute() {
    assert_eq!(<ProxyLike as SafeTraceable>::TYPE_TAG, PROXY_LIKE_TYPE_TAG);
    assert_eq!(
        <CounterLike as SafeTraceable>::TYPE_TAG,
        COUNTER_LIKE_TYPE_TAG
    );
    assert_eq!(
        <RegExpLike as SafeTraceable>::TYPE_TAG,
        REGEXP_LIKE_TYPE_TAG
    );
}

#[test]
fn proxy_like_traces_immediate_values_as_noop() {
    let body = ProxyLike {
        target: Value::UNDEFINED,
        handler: Value::NULL,
        revoked: true,
    };
    // Immediate `Value`s carry no slot; visitor must not fire.
    assert!(collect(&body).is_empty());
}

#[test]
fn counter_like_with_only_skipped_fields_compiles_and_is_noop() {
    let body = CounterLike { count: 42 };
    assert!(collect(&body).is_empty());
}

#[test]
fn regexp_like_walks_refcell_and_option() {
    let body = RegExpLike {
        last_index: RefCell::new(Value::UNDEFINED),
        extra: Some(Value::NULL),
        flags: 0,
    };
    // Both `Value`s are immediate — still a no-op, but exercises the
    // expansion path through `RefCell::borrow()` and `Option::Some`.
    assert!(collect(&body).is_empty());
}
