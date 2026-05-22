//! Integration coverage for the `raft!` grouped spec macro.

use otter_macros::raft;
use otter_vm::object;
use otter_vm::{Interpreter, NamespaceBuilder, Value};

mod raft_ns {
    use otter_vm::{NativeCtx, NativeError, Value};

    pub fn one(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(1))
    }

    pub fn two(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
        Ok(Value::number_i32(2))
    }
}

raft! {
    pub static RAFT_NS_SPEC: namespace("RaftNs") {
        methods: [
            "one" => raft_ns::one, length = 0;
            "two" => raft_ns::two, length = 1;
        ]
    }
}

#[test]
fn raft_generates_grouped_static_namespace_spec() {
    assert_eq!(RAFT_NS_SPEC.name, "RaftNs");
    assert_eq!(RAFT_NS_SPEC.methods.len(), 2);
    assert!(RAFT_NS_SPEC.accessors.is_empty());
    assert!(RAFT_NS_SPEC.constants.is_empty());

    let mut interp = Interpreter::new();
    let ns = NamespaceBuilder::from_spec(interp.gc_heap_mut(), &RAFT_NS_SPEC)
        .expect("builder")
        .build()
        .expect("namespace");

    let Value::NativeFunction(one) = object::get(ns, interp.gc_heap(), "one").expect("one") else {
        panic!("one should be native");
    };
    assert!(one.is_static_call(interp.gc_heap()));
    assert_eq!(one.length(interp.gc_heap()), 0);

    let Value::NativeFunction(two) = object::get(ns, interp.gc_heap(), "two").expect("two") else {
        panic!("two should be native");
    };
    assert!(two.is_static_call(interp.gc_heap()));
    assert_eq!(two.length(interp.gc_heap()), 1);
}
