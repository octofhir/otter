//! Identity and leaf-entry declarations for guarded static-native JIT calls.
//!
//! Ordinary-call feedback records one runtime-stub id after resolving an exact
//! bootstrap native. This module is the single neutral registry joining that
//! JavaScript-callable identity to its leaf ABI entry and exact supported
//! argument count; generated code does not maintain a second builtin taxonomy.
//!
//! # Contents
//! - [`JitLeafBuiltin`] — one guarded bootstrap-native declaration.
//! - [`jit_static_call_target`] — exact callable classification for feedback.
//! - [`jit_leaf_builtin`] and [`jit_static_call_ref`] — compile-time lookup.
//!
//! # Invariants
//! - Classification compares the original static native function identity,
//!   never a property name or mutable global slot.
//! - A declaration's argument count is exact. All other call shapes retain the
//!   canonical ordinary-call path.
//! - A declaration with [`JitLeafBuiltin::this_operand`] reads the call's
//!   `this` as its first operand word. Only an explicit-receiver call
//!   (`CallWithThis`) may lower it; plain calls and shaped method sites, which
//!   pass no receiver word, keep the canonical path for it. Its entry proves
//!   the receiver's own type and misses on anything else.
//! - Every declared leaf id resolves through the shared runtime-stub inventory.
//!
//! # See also
//! - [`crate::runtime_stubs`] for machine-callable leaf implementations.
//! - [`crate::feedback`] for ordinary-call target feedback.

use crate::native_function::NativeFastFn;

/// One bootstrap native a generated call site may reach through a declared
/// leaf entry instead of a materialized frame.
///
/// The declared entry id is this row's identity everywhere downstream:
/// feedback, relocations, and diagnostics all carry it. The guarded callable
/// identity remains private to this registry.
pub struct JitLeafBuiltin {
    identity: BootstrapNative,
    /// Declared entry the guarded site calls once its guards pass.
    pub leaf_stub_id: crate::native_abi::RuntimeStubId,
    /// Exact JavaScript argument count the entry implements.
    pub argument_count: u8,
    /// Whether the entry reads the call's `this` as its first operand word,
    /// ahead of the arguments. The entry itself proves that receiver.
    pub this_operand: bool,
}

impl JitLeafBuiltin {
    /// Operand words the entry reads at a site that passes `this` when
    /// [`Self::this_operand`] asks for it.
    #[must_use]
    pub fn operand_words(&self) -> usize {
        usize::from(self.this_operand) + usize::from(self.argument_count)
    }
}

#[derive(Clone, Copy)]
enum BootstrapNative {
    Math(otter_bytecode::method_id::MathMethod),
    ParseInt,
    StringPrototype(&'static str),
    MapPrototype(&'static str),
    SetPrototype(&'static str),
}

impl BootstrapNative {
    fn original_native_fn(self) -> NativeFastFn {
        match self {
            Self::Math(method) => crate::math::original_native_fn(method),
            Self::ParseInt => crate::intrinsics::number::number_parse_int_native,
            Self::StringPrototype(name) => crate::string::prototype::prototype_bridge(name)
                .expect("declared String.prototype leaf has a bridge"),
            Self::MapPrototype(name) => crate::bootstrap_collections::map_prototype_native(name)
                .expect("declared Map.prototype leaf is installed"),
            Self::SetPrototype(name) => crate::bootstrap_collections::set_prototype_native(name)
                .expect("declared Set.prototype leaf is installed"),
        }
    }
}

/// One `this`-reading declaration.
const fn receiver_leaf(
    identity: BootstrapNative,
    leaf_stub_id: crate::native_abi::RuntimeStubId,
) -> JitLeafBuiltin {
    JitLeafBuiltin {
        identity,
        leaf_stub_id,
        argument_count: 1,
        this_operand: true,
    }
}

/// Bootstrap natives supported by guarded leaf-call codegen.
///
/// One row per builtin. Adding a builtin extends this table and the shared
/// runtime-stub inventory; neither feedback nor generated-call code names it.
const JIT_LEAF_BUILTINS: &[JitLeafBuiltin] = &[
    JitLeafBuiltin {
        identity: BootstrapNative::Math(otter_bytecode::method_id::MathMethod::Abs),
        leaf_stub_id: crate::native_abi::STUB_MATH_ABS_LEAF.id,
        argument_count: 1,
        this_operand: false,
    },
    JitLeafBuiltin {
        identity: BootstrapNative::Math(otter_bytecode::method_id::MathMethod::Floor),
        leaf_stub_id: crate::native_abi::STUB_MATH_FLOOR_LEAF.id,
        argument_count: 1,
        this_operand: false,
    },
    JitLeafBuiltin {
        identity: BootstrapNative::Math(otter_bytecode::method_id::MathMethod::Sqrt),
        leaf_stub_id: crate::native_abi::STUB_MATH_SQRT_LEAF.id,
        argument_count: 1,
        this_operand: false,
    },
    JitLeafBuiltin {
        identity: BootstrapNative::Math(otter_bytecode::method_id::MathMethod::Max),
        leaf_stub_id: crate::native_abi::STUB_MATH_MAX_LEAF.id,
        argument_count: 2,
        this_operand: false,
    },
    JitLeafBuiltin {
        identity: BootstrapNative::Math(otter_bytecode::method_id::MathMethod::Min),
        leaf_stub_id: crate::native_abi::STUB_MATH_MIN_LEAF.id,
        argument_count: 2,
        this_operand: false,
    },
    JitLeafBuiltin {
        identity: BootstrapNative::ParseInt,
        leaf_stub_id: crate::native_abi::STUB_PARSE_INT_I32_LEAF.id,
        argument_count: 1,
        this_operand: false,
    },
    receiver_leaf(
        BootstrapNative::StringPrototype("charCodeAt"),
        crate::native_abi::STUB_STRING_CHAR_CODE_AT_LEAF.id,
    ),
    receiver_leaf(
        BootstrapNative::StringPrototype("codePointAt"),
        crate::native_abi::STUB_STRING_CODE_POINT_AT_LEAF.id,
    ),
    receiver_leaf(
        BootstrapNative::StringPrototype("indexOf"),
        crate::native_abi::STUB_STRING_INDEX_OF_LEAF.id,
    ),
    receiver_leaf(
        BootstrapNative::StringPrototype("includes"),
        crate::native_abi::STUB_STRING_INCLUDES_LEAF.id,
    ),
    receiver_leaf(
        BootstrapNative::StringPrototype("startsWith"),
        crate::native_abi::STUB_STRING_STARTS_WITH_LEAF.id,
    ),
    receiver_leaf(
        BootstrapNative::StringPrototype("endsWith"),
        crate::native_abi::STUB_STRING_ENDS_WITH_LEAF.id,
    ),
    receiver_leaf(
        BootstrapNative::MapPrototype("get"),
        crate::native_abi::STUB_COLLECTION_MAP_GET_LEAF.id,
    ),
    receiver_leaf(
        BootstrapNative::MapPrototype("has"),
        crate::native_abi::STUB_COLLECTION_MAP_HAS_LEAF.id,
    ),
    receiver_leaf(
        BootstrapNative::SetPrototype("has"),
        crate::native_abi::STUB_COLLECTION_SET_HAS_LEAF.id,
    ),
];

/// Classify a callee against the guarded leaf-callable builtin table.
pub(crate) fn jit_static_call_target(
    native: crate::NativeFunction,
    heap: &otter_gc::GcHeap,
) -> Option<&'static JitLeafBuiltin> {
    for row in JIT_LEAF_BUILTINS {
        if native.is_static_fn(heap, row.identity.original_native_fn()) {
            debug_assert_eq!(
                native.native_ref(heap),
                jit_static_call_ref(row.leaf_stub_id, heap),
                "static-native classifier and JIT identity field must agree"
            );
            return Some(row);
        }
    }
    None
}

/// Declaration behind a leaf entry id, or `None` when the id is not a guarded
/// callable builtin.
#[must_use]
pub fn jit_leaf_builtin(
    stub_id: crate::native_abi::RuntimeStubId,
) -> Option<&'static JitLeafBuiltin> {
    JIT_LEAF_BUILTINS
        .iter()
        .find(|row| row.leaf_stub_id == stub_id)
}

/// External-reference index of the exact bootstrap function guarded before a
/// declared leaf runs.
///
/// Returns `None` when the id has no declaration or this isolate never
/// installed the builtin. In either case the site stays generic.
#[must_use]
pub fn jit_static_call_ref(
    stub_id: crate::native_abi::RuntimeStubId,
    heap: &otter_gc::GcHeap,
) -> Option<u32> {
    let row = jit_leaf_builtin(stub_id)?;
    heap.external_refs()
        .lookup(row.identity.original_native_fn() as *const () as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_stub_id_has_no_static_call_ref() {
        let heap = otter_gc::GcHeap::new().expect("gc heap");
        assert_eq!(jit_static_call_ref(u32::MAX, &heap), None);
    }

    #[test]
    fn every_declaration_resolves_a_leaf_that_fits_its_operand_words() {
        let interpreter = crate::Interpreter::new();
        for row in JIT_LEAF_BUILTINS {
            let _ = row.identity.original_native_fn();
            assert!(
                crate::runtime_stubs::leaf_no_alloc_stub2_by_id(row.leaf_stub_id)
                    .is_some_and(|stub| stub.is_valid()),
                "{} must name a valid no-allocation leaf",
                crate::native_abi::runtime_stub_name(row.leaf_stub_id)
            );
            assert!(row.operand_words() <= 2);
            assert!(
                jit_static_call_ref(row.leaf_stub_id, &interpreter.gc_heap).is_some(),
                "{} must be installed in a fresh isolate",
                crate::native_abi::runtime_stub_name(row.leaf_stub_id)
            );
        }
        assert!(JIT_LEAF_BUILTINS.iter().any(|row| row.this_operand));
    }
}
