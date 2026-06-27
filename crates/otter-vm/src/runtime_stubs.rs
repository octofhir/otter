//! VM-native runtime stub entrypoints.
//!
//! These functions are the reusable implementation layer behind
//! [`crate::native_abi`] descriptors. The current interpreter can call them
//! directly, and generated code can later call the same entrypoints instead of
//! reimplementing equivalent fast paths.
//!
//! # Contents
//! - Leaf/no-allocation collection probes for `Map.get`, `Map.has`, and
//!   `Set.has`.
//!
//! # Invariants
//! - Arguments are boxed [`crate::Value`] raw ABI bits.
//! - Results are returned as [`crate::native_abi::RuntimeStubResult`].
//! - `LeafNoAlloc` stubs must not allocate, trigger GC, call JS, flatten
//!   strings, or mutate heap state.
//!
//! # See also
//! - [`crate::native_abi`]
//! - [`crate::method_ops`]

use crate::native_abi::{
    NO_SAFEPOINT, RuntimeStubDescriptor, RuntimeStubResult, STUB_COLLECTION_MAP_GET_LEAF,
    STUB_COLLECTION_MAP_HAS_LEAF, STUB_COLLECTION_SET_HAS_LEAF, validate_stub_descriptor,
};
use crate::{Value, collections};

/// Two-argument leaf/no-allocation runtime stub ABI.
pub type LeafNoAllocStub2Fn = fn(&otter_gc::GcHeap, u64, u64) -> RuntimeStubResult;

/// Callable leaf/no-allocation stub entry with its ABI descriptor.
#[derive(Clone, Copy)]
pub struct LeafNoAllocStub2 {
    /// Passive descriptor shared with profiler/JIT metadata.
    pub descriptor: RuntimeStubDescriptor,
    /// Machine-callable Rust entrypoint with the descriptor's fixed ABI shape.
    pub entry: LeafNoAllocStub2Fn,
}

impl LeafNoAllocStub2 {
    /// `true` when descriptor metadata matches this callable ABI shape.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        validate_stub_descriptor(self.descriptor, NO_SAFEPOINT)
            && self.descriptor.argument_count == 2
    }
}

/// Callable ABI entry for `Map.prototype.get`.
pub const COLLECTION_MAP_GET_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_COLLECTION_MAP_GET_LEAF,
    entry: collection_map_get_leaf,
};

/// Callable ABI entry for `Map.prototype.has`.
pub const COLLECTION_MAP_HAS_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_COLLECTION_MAP_HAS_LEAF,
    entry: collection_map_has_leaf,
};

/// Callable ABI entry for `Set.prototype.has`.
pub const COLLECTION_SET_HAS_LEAF: LeafNoAllocStub2 = LeafNoAllocStub2 {
    descriptor: STUB_COLLECTION_SET_HAS_LEAF,
    entry: collection_set_has_leaf,
};

/// Leaf `Map.prototype.get` probe.
///
/// Returns `Miss` when the receiver is not a Map or the key would need string
/// materialisation/flattening before a no-GC lookup is safe.
#[must_use]
pub fn collection_map_get_leaf(
    heap: &otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
) -> RuntimeStubResult {
    let recv = Value::from_abi_bits(recv_bits);
    let key = Value::from_abi_bits(key_bits);
    if !leaf_key_is_materialized(heap, key) {
        return RuntimeStubResult::miss();
    }
    let Some(map) = recv.as_map() else {
        return RuntimeStubResult::miss();
    };
    RuntimeStubResult::ok_value(
        collections::map_get(map, heap, &key).unwrap_or_else(Value::undefined),
    )
}

/// Leaf `Map.prototype.has` probe.
///
/// Returns `Miss` when the receiver is not a Map or the key would need string
/// materialisation/flattening before a no-GC lookup is safe.
#[must_use]
pub fn collection_map_has_leaf(
    heap: &otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
) -> RuntimeStubResult {
    let recv = Value::from_abi_bits(recv_bits);
    let key = Value::from_abi_bits(key_bits);
    if !leaf_key_is_materialized(heap, key) {
        return RuntimeStubResult::miss();
    }
    let Some(map) = recv.as_map() else {
        return RuntimeStubResult::miss();
    };
    RuntimeStubResult::ok_value(Value::boolean(collections::map_has(map, heap, &key)))
}

/// Leaf `Set.prototype.has` probe.
///
/// Returns `Miss` when the receiver is not a Set or the key would need string
/// materialisation/flattening before a no-GC lookup is safe.
#[must_use]
pub fn collection_set_has_leaf(
    heap: &otter_gc::GcHeap,
    recv_bits: u64,
    key_bits: u64,
) -> RuntimeStubResult {
    let recv = Value::from_abi_bits(recv_bits);
    let key = Value::from_abi_bits(key_bits);
    if !leaf_key_is_materialized(heap, key) {
        return RuntimeStubResult::miss();
    }
    let Some(set) = recv.as_set() else {
        return RuntimeStubResult::miss();
    };
    RuntimeStubResult::ok_value(Value::boolean(collections::set_has(set, heap, &key)))
}

fn leaf_key_is_materialized(heap: &otter_gc::GcHeap, key: Value) -> bool {
    key.as_string(heap)
        .is_none_or(|string| string.is_flat_or_latin1(heap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_abi::RuntimeStubStatus;

    fn n(i: i32) -> Value {
        Value::number_i32(i)
    }

    #[test]
    fn leaf_stub_entries_match_descriptors() {
        assert!(COLLECTION_MAP_GET_LEAF.is_valid());
        assert!(COLLECTION_MAP_HAS_LEAF.is_valid());
        assert!(COLLECTION_SET_HAS_LEAF.is_valid());
    }

    #[test]
    fn map_get_leaf_hits_flat_key() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let map = collections::alloc_map(&mut heap).expect("map");
        let key = crate::string::JsString::from_str("k", &mut heap).expect("key");
        collections::map_set(map, &mut heap, Value::string(key), n(42)).expect("set");

        let result = collection_map_get_leaf(
            &heap,
            Value::map(map).to_abi_bits(),
            Value::string(key).to_abi_bits(),
        );
        assert_eq!(result.status, RuntimeStubStatus::Ok);
        assert_eq!(result.into_value(), Some(n(42)));
    }

    #[test]
    fn map_has_leaf_misses_rope_key() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let map = collections::alloc_map(&mut heap).expect("map");
        let left = crate::string::JsString::from_str("k", &mut heap).expect("left");
        let right = crate::string::JsString::from_str("1", &mut heap).expect("right");
        let rope = crate::string::JsString::concat(left, right, &mut heap).expect("rope");

        let result = collection_map_has_leaf(
            &heap,
            Value::map(map).to_abi_bits(),
            Value::string(rope).to_abi_bits(),
        );
        assert_eq!(result.status, RuntimeStubStatus::Miss);
        assert_eq!(result.into_value(), None);
    }

    #[test]
    fn set_has_leaf_hits_flat_key() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let set = collections::alloc_set(&mut heap).expect("set");
        collections::set_add(set, &mut heap, n(7)).expect("add");

        let result =
            collection_set_has_leaf(&heap, Value::set(set).to_abi_bits(), n(7).to_abi_bits());
        assert_eq!(result.status, RuntimeStubStatus::Ok);
        assert_eq!(result.into_value(), Some(Value::boolean(true)));
    }
}
