//! Method-call inline-cache and fast-dispatch feedback types.
//!
//! Cohesive data carried by the `CallMethodValue` dispatch in the parent
//! module: which Map/Set builtin a site resolved to ([`CollectionFastOp`] /
//! [`CollectionFastTarget`]) and the monomorphic method-call cache entries
//! ([`MethodCallIc`] and its array/collection variants). All hold only non-GC
//! metadata validated by native function identity, so none of them are traced.
//!
//! # See also
//! - [`super`] — the dispatch that builds and replays these.

use crate::Value;
use crate::native_abi::RuntimeStubId;

/// Which Map/Set builtin the direct dispatch resolved to.
#[derive(Clone, Copy)]
pub(crate) enum CollectionFastOp {
    MapGet,
    MapSet,
    MapHas,
    MapDelete,
    SetAdd,
    SetHas,
    SetDelete,
}

impl CollectionFastOp {
    pub(crate) fn from_map_name(name: &str) -> Option<Self> {
        match name {
            "get" => Some(Self::MapGet),
            "set" => Some(Self::MapSet),
            "has" => Some(Self::MapHas),
            "delete" => Some(Self::MapDelete),
            _ => None,
        }
    }

    pub(crate) fn from_set_name(name: &str) -> Option<Self> {
        match name {
            "add" => Some(Self::SetAdd),
            "has" => Some(Self::SetHas),
            "delete" => Some(Self::SetDelete),
            _ => None,
        }
    }

    pub(crate) fn is_map(self) -> bool {
        matches!(
            self,
            Self::MapGet | Self::MapSet | Self::MapHas | Self::MapDelete
        )
    }

    pub(crate) fn is_set(self) -> bool {
        matches!(self, Self::SetAdd | Self::SetHas | Self::SetDelete)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::MapGet => "get",
            Self::MapSet => "set",
            Self::MapHas => "has",
            Self::MapDelete => "delete",
            Self::SetAdd => "add",
            Self::SetHas => "has",
            Self::SetDelete => "delete",
        }
    }

    pub(crate) fn matches_builtin(self, method: Value, heap: &otter_gc::GcHeap) -> bool {
        if self.is_map() {
            crate::bootstrap_collections::is_map_prototype_builtin(method, heap, self.name())
        } else {
            crate::bootstrap_collections::is_set_prototype_builtin(method, heap, self.name())
        }
    }

    pub(crate) fn leaf_stub_id(self) -> Option<RuntimeStubId> {
        match self {
            Self::MapGet => Some(crate::native_abi::STUB_COLLECTION_MAP_GET_LEAF.id),
            Self::MapHas => Some(crate::native_abi::STUB_COLLECTION_MAP_HAS_LEAF.id),
            Self::SetHas => Some(crate::native_abi::STUB_COLLECTION_SET_HAS_LEAF.id),
            Self::MapSet | Self::MapDelete | Self::SetAdd | Self::SetDelete => None,
        }
    }

    pub(crate) fn alloc_stub_id(self) -> Option<RuntimeStubId> {
        match self {
            Self::MapGet => Some(crate::native_abi::STUB_COLLECTION_MAP_GET_ALLOC.id),
            Self::MapHas => Some(crate::native_abi::STUB_COLLECTION_MAP_HAS_ALLOC.id),
            Self::MapSet => Some(crate::native_abi::STUB_COLLECTION_MAP_SET_ALLOC.id),
            Self::SetAdd => Some(crate::native_abi::STUB_COLLECTION_SET_ADD_ALLOC.id),
            Self::SetHas => Some(crate::native_abi::STUB_COLLECTION_SET_HAS_ALLOC.id),
            Self::MapDelete => Some(crate::native_abi::STUB_COLLECTION_MAP_DELETE_ALLOC.id),
            Self::SetDelete => Some(crate::native_abi::STUB_COLLECTION_SET_DELETE_ALLOC.id),
        }
    }
}

/// Resolved collection builtin target carried by method-call feedback.
#[derive(Clone, Copy)]
pub(crate) struct CollectionFastTarget {
    pub(crate) op: CollectionFastOp,
    pub(crate) leaf_stub_id: Option<RuntimeStubId>,
}

impl CollectionFastTarget {
    pub(crate) fn new(op: CollectionFastOp) -> Self {
        Self {
            op,
            leaf_stub_id: op.leaf_stub_id(),
        }
    }
}

/// Monomorphic method-call inline cache entry.
///
/// Entries keep only non-GC metadata: prototype shape, prototype slot, and a
/// stable builtin tag/op. The hot guard re-reads the slot from the realm
/// prototype and validates the builtin by native function identity.
#[derive(Clone, Copy)]
pub(crate) enum MethodCallIc {
    Array(ArrayMethodCallIc),
    Collection(CollectionMethodCallIc),
}

/// Monomorphic method-call inline cache entry for a dense-array builtin site.
///
/// Records the `%Array.prototype%` shape and the own-slot offset that resolved
/// `tag`'s method, so a re-validating guard reads the slot directly (no key
/// hash) and confirms it still holds the original builtin by function pointer.
/// Holds no GC pointer — `proto_shape`/`proto_slot` are plain metadata and the
/// builtin identity is checked against a stable native `fn` address — so the
/// cache needs no tracing and can never dangle across a scavenge.
#[derive(Clone, Copy)]
pub(crate) struct ArrayMethodCallIc {
    pub(crate) proto_shape: crate::object::ShapeId,
    pub(crate) proto_slot: u16,
    pub(crate) tag: crate::array_prototype::ArrayMethodTag,
}

/// Monomorphic method-call inline cache entry for a Map/Set builtin site.
///
/// The receiver family and prototype/expando guards are checked before the
/// cached slot is trusted. Shape + slot are enough to skip the prototype slot
/// lookup and method-name dispatch on the steady-state hot path.
#[derive(Clone, Copy)]
pub(crate) struct CollectionMethodCallIc {
    pub(crate) proto_shape: crate::object::ShapeId,
    pub(crate) proto_slot: u16,
    pub(crate) op: CollectionFastOp,
    pub(crate) leaf_stub_id: Option<RuntimeStubId>,
    pub(crate) alloc_stub_id: Option<RuntimeStubId>,
}
