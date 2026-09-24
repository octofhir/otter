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
//! - Every declared leaf id resolves through the shared runtime-stub inventory
//!   ([`crate::runtime_stubs::leaf_entry_shape`]): pure reads, or in-place
//!   writes that carry their own barrier and miss instead of allocating.
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
    /// Original bootstrap function a callable must still run.
    identity: NativeFastFn,
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

/// One declaration whose entry reads only its arguments.
const fn argument_leaf(
    identity: NativeFastFn,
    leaf_stub_id: crate::native_abi::RuntimeStubId,
    argument_count: u8,
) -> JitLeafBuiltin {
    JitLeafBuiltin {
        identity,
        leaf_stub_id,
        argument_count,
        this_operand: false,
    }
}

/// One declaration whose entry reads `this` and one argument.
const fn receiver_leaf(
    identity: NativeFastFn,
    leaf_stub_id: crate::native_abi::RuntimeStubId,
) -> JitLeafBuiltin {
    JitLeafBuiltin {
        identity,
        leaf_stub_id,
        argument_count: 1,
        this_operand: true,
    }
}

/// One declaration whose in-place entry reads `this` and two arguments.
const fn receiver_leaf2(
    identity: NativeFastFn,
    leaf_stub_id: crate::native_abi::RuntimeStubId,
) -> JitLeafBuiltin {
    JitLeafBuiltin {
        identity,
        leaf_stub_id,
        argument_count: 2,
        this_operand: true,
    }
}

/// Bootstrap natives supported by guarded leaf-call codegen.
///
/// One row per builtin. Adding a builtin extends this table and the shared
/// runtime-stub inventory; neither feedback nor generated-call code names it.
const JIT_LEAF_BUILTINS: &[JitLeafBuiltin] = {
    use crate::native_abi as abi;
    use crate::string::prototype as string;
    &[
        argument_leaf(crate::math::native_abs, abi::STUB_MATH_ABS_LEAF.id, 1),
        argument_leaf(crate::math::native_floor, abi::STUB_MATH_FLOOR_LEAF.id, 1),
        argument_leaf(crate::math::native_sqrt, abi::STUB_MATH_SQRT_LEAF.id, 1),
        argument_leaf(crate::math::native_max, abi::STUB_MATH_MAX_LEAF.id, 2),
        argument_leaf(crate::math::native_min, abi::STUB_MATH_MIN_LEAF.id, 2),
        argument_leaf(
            crate::intrinsics::number::number_parse_int_native,
            abi::STUB_PARSE_INT_I32_LEAF.id,
            1,
        ),
        receiver_leaf(
            string::bridge_char_code_at,
            abi::STUB_STRING_CHAR_CODE_AT_LEAF.id,
        ),
        receiver_leaf(
            string::bridge_code_point_at,
            abi::STUB_STRING_CODE_POINT_AT_LEAF.id,
        ),
        receiver_leaf(string::bridge_index_of, abi::STUB_STRING_INDEX_OF_LEAF.id),
        receiver_leaf(string::bridge_includes, abi::STUB_STRING_INCLUDES_LEAF.id),
        receiver_leaf(
            string::bridge_starts_with,
            abi::STUB_STRING_STARTS_WITH_LEAF.id,
        ),
        receiver_leaf(string::bridge_ends_with, abi::STUB_STRING_ENDS_WITH_LEAF.id),
        receiver_leaf(
            crate::bootstrap_collections::map_proto_get,
            abi::STUB_COLLECTION_MAP_GET_LEAF.id,
        ),
        // Overwrites an existing key in place; an insertion misses.
        receiver_leaf2(
            crate::bootstrap_collections::map_proto_set,
            abi::STUB_COLLECTION_MAP_SET_MUTATING.id,
        ),
        receiver_leaf(
            crate::bootstrap_collections::map_proto_has,
            abi::STUB_COLLECTION_MAP_HAS_LEAF.id,
        ),
        receiver_leaf(
            crate::bootstrap_collections::set_proto_has,
            abi::STUB_COLLECTION_SET_HAS_LEAF.id,
        ),
    ]
};

/// Classify a callee against the guarded leaf-callable builtin table.
///
/// Reads the callable's static entry once and compares function addresses;
/// this runs on every unsaturated generic call's feedback path.
pub(crate) fn jit_static_call_target(
    native: crate::NativeFunction,
    heap: &otter_gc::GcHeap,
) -> Option<&'static JitLeafBuiltin> {
    let entry = native.static_fn(heap)?;
    let row = JIT_LEAF_BUILTINS
        .iter()
        .find(|row| std::ptr::fn_addr_eq(row.identity, entry))?;
    debug_assert_eq!(
        native.native_ref(heap),
        jit_static_call_ref(row.leaf_stub_id, heap),
        "static-native classifier and JIT identity field must agree"
    );
    Some(row)
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
        .lookup(row.identity as *const () as usize)
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
            let shape = crate::runtime_stubs::leaf_entry_shape(row.leaf_stub_id);
            assert!(
                shape.is_some_and(|shape| row.operand_words() <= usize::from(shape.words)),
                "{} must name a valid non-allocating leaf wide enough for its operands",
                crate::native_abi::runtime_stub_name(row.leaf_stub_id)
            );
            // Only explicit-receiver sites may use an entry that writes.
            assert!(!shape.is_some_and(|shape| shape.mutates) || row.this_operand);
            assert!(
                jit_static_call_ref(row.leaf_stub_id, &interpreter.gc_heap).is_some(),
                "{} must be installed in a fresh isolate",
                crate::native_abi::runtime_stub_name(row.leaf_stub_id)
            );
        }
        assert!(JIT_LEAF_BUILTINS.iter().any(|row| row.this_operand));
    }
}
