//! AArch64 machine-code backend for the baseline pipeline.
//!
//! # Contents
//! - Dynasm templates for supported bytecode operations.
//! - Function, bailout, runtime-stub, and OSR emission.
//! - AArch64-specific guards, boxing, and inline-cache fast paths.
//!
//! # Invariants
//! - Input has passed the backend-neutral BaselinePlan prepass.
//! - Allocating calls use planned safepoints and published frame roots.
//! - Embedded data pointers are retained by EmissionArtifacts.
//!
//! # See also
//! - super::lowering for validation and planning.
//! - super::code for finalized code ownership and VM entry.

#![allow(unused_parens)]
use super::{
    ALLOC_CTX_CODE_OBJECT_ID_OFFSET, ALLOC_CTX_FRAME_OFFSET, ALLOC_CTX_RESERVED0_OFFSET,
    ALLOC_CTX_RESERVED1_OFFSET, ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET,
    ALLOC_CTX_SPILL_SLOTS_OFFSET, ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET,
    ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET, BACKEDGE_FUEL_OFFSET, BaselineCode, BaselinePlan,
    CANONICAL_NAN_HI16, COLLECTION_METHOD_IC_ALLOC_STUB_ID_OFFSET,
    COLLECTION_METHOD_IC_BUILTIN_FN_ADDR_OFFSET, COLLECTION_METHOD_IC_COUNT_OFFSET,
    COLLECTION_METHOD_IC_LEAF_STUB_ID_OFFSET, COLLECTION_METHOD_IC_METHOD_VALUE_BYTE_OFFSET,
    COLLECTION_METHOD_IC_PROTO_OFFSET, COLLECTION_METHOD_IC_PROTO_SHAPE_OFFSET,
    COLLECTION_METHOD_IC_RECEIVER_TYPE_TAG_OFFSET, COLLECTION_METHOD_IC_SLOT_SIZE,
    COLLECTION_METHOD_IC_STATE_OFFSET, COLLECTION_METHOD_ICS_OFFSET, DIRECT_ENTRY_OFFSET,
    DIRECT_FRAME_INDEX_OFFSET, DIRECT_METHOD_ENTRY_OFFSET, DIRECT_METHOD_FID_OFFSET,
    DIRECT_METHOD_INLINE_OFFSET, DIRECT_METHOD_INLINE_SLOT_SIZE, DIRECT_METHOD_ON_RECEIVER_OFFSET,
    DIRECT_METHOD_PROTO_SHAPE_OFFSET, DIRECT_METHOD_RECV_SHAPE_OFFSET,
    DIRECT_METHOD_REGISTER_COUNT_OFFSET, DIRECT_METHOD_VALUE_BYTE_OFFSET, DIRECT_REGS_OFFSET,
    DIRECT_SELF_OFFSET, DIRECT_THIS_OFFSET, DIRECT_UPVALUES_OFFSET, DOUBLE_OFFSET_HI16,
    ERROR_SLOT_OFFSET, EmissionArtifacts, FRAME_INDEX_OFFSET, FUNCTION_ID_TAG, GC_HEAP_OFFSET,
    IC_WAYS, INTERRUPT_FLAG_OFFSET, JIT_CTX_STACK_SIZE, JS_CLOSURE_BODY_TYPE_TAG, MAX_INLINE_ARGS,
    MAX_METHOD_ARGS, NATIVE_FRAME_CODE_OBJECT_ID_OFFSET, NATIVE_FRAME_OFFSET, NUMBER_TAG_HI16,
    OBJECT_BODY_TYPE_TAG, Op, REG_STACK_BASE_OFFSET, REG_TOP_PTR_OFFSET, RESUME_PC_OFFSET,
    STATUS_BAILED, STATUS_RETURNED, STATUS_THREW, SYNC_REENTRY_DEPTH_PTR_OFFSET,
    SYNC_REENTRY_LIMIT_OFFSET, THIS_VALUE_OFFSET, THREAD_OFFSET, UPVALUE_CELL_SIZE,
    UPVALUE_VALUE_OFFSET, UPVALUES_PTR_OFFSET, Unsupported, VALUE_FALSE, VALUE_FALSE_LOW,
    VALUE_HOLE, VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED, WordOperands,
    alloc_value_stub_trampoline_pair, branch_target, const_index, imm32,
    jit_abort_direct_call_stub, jit_add_stub, jit_backedge_poll_stub,
    jit_call_collection_method_ic_stub, jit_define_data_property_stub,
    jit_define_own_property_stub, jit_direct_method_call_bail_stub,
    jit_finish_direct_call_bailed_stub, jit_finish_direct_call_returned_stub,
    jit_fresh_upvalue_stub, jit_inline_closure_upvalues_stub, jit_load_builtin_error_stub,
    jit_load_element_stub, jit_load_global_stub, jit_load_prop_window_stub, jit_load_string_stub,
    jit_load_upvalue_stub, jit_make_closure_stub, jit_make_fn_stub, jit_math_call_stub,
    jit_neg_stub, jit_new_array_stub, jit_new_object_stub, jit_pop_native_activation_stub,
    jit_prepare_direct_method_call_stub, jit_push_native_activation_stub, jit_self_call_bail_stub,
    jit_store_element_stub, jit_store_prop_window_stub, jit_store_upvalue_checked_stub,
    jit_store_upvalue_stub, jit_write_barrier_stub, jit_write_barrier_window_stub,
    leaf_no_alloc_stub2_trampoline_pair, local_index, otter_jit_math_random, pack_method_arg_regs,
    reg, reg_offset, reg3, value_tag,
};
use crate::CompiledCode;
use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::Interpreter;
use otter_vm::{
    JitArrayMethod, JitArrayMethodKind, JitCollectionAllocMethod, JitCollectionLeafMethod,
    JitCompileSnapshot, JitInlineCallee, JitInlineMethod, JitTypedArrayLayout,
    STUB_COLLECTION_SET_ADD_ALLOC, STUB_STRING_CONCAT_ALLOC, SafepointId,
    jit::{JIT_COLLECTION_METHOD_IC_COLLECTION, JIT_COLLECTION_METHOD_IC_NO_STUB},
    runtime_stubs::alloc_value_stub_by_id,
};
use std::collections::BTreeMap;

#[macro_use]
mod arithmetic;
mod assembler;
mod calls;
mod control;
mod elements;
mod emitter;
use arithmetic::*;
use assembler::*;
use calls::*;
use control::*;
pub(super) use elements::vec_layout_offsets;
use elements::{emit_array_store, emit_element_load, emit_ta_guard_chain};
pub(super) use emitter::compile;
