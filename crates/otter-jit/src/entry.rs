//! Shared native-compilation infrastructure for compiled tiers.
//!
//! Owns everything the machine backend consumes that is not itself a code
//! tiers: the frozen compiled-entry ABI (`JitCtx`/`NativeResultPair` and its
//! offsets), the boxed-value encoding constants, the backend-neutral typed
//! lowering plan, the classified runtime-stub entries, and the shared VM
//! entry path ([`enter_compiled`]).
//!
//! # Contents
//! - [`abi`] — entry context layouts and baked field offsets.
//! - [`value_abi`] — frozen JS value tag constants and pre-split immediates.
//! - [`lowering`] — [`BaselinePlan`] typed instruction stream over bytecode.
//! - [`runtime_ops`] — typed C ABI runtime transition entries.
//! - [`code`] — [`enter_compiled`], the shared activation-to-entry invocation.
//! - [`runtime_stub_bindings`] — the complete active JIT-owned transition
//!   inventory.
//! - [`TransitionTable`] — hook-lifetime descriptor-id-indexed resolution of
//!   that inventory for O(1) compile-time address baking.
//!
//! # Invariants
//! - Compiled functions return the VM-owned two-word `NativeResultPair`.
//!   `Return` carries a value, `Bail` carries the exact published logical PC,
//!   `Throw` carries a pure exception value, and only `Fatal` consults
//!   `ctx.error`.
//! - Interpreter-visible registers remain in the published frame window at
//!   every side exit and allocating/reentrant call; no movable JS pointer is
//!   kept only in a machine register across a safepoint.
//! - Runtime entries install through validated descriptor bindings; every
//!   active JIT-owned slot is filled, so emitted and audited stub sets cannot
//!   drift.
//!
//! # See also
//! - [`crate::template`] — the baseline compiler consuming this module.
//! - [`crate::optimizing`] — the feedback-guided optimizing consumer.
//! - `JIT_DESIGN.md` §3.2 (backend), §3.5 (GC contract).

mod abi;
mod code;
mod lowering;
mod runtime_ops;
mod value_abi;
pub(crate) use abi::*;
pub(crate) use code::enter_compiled;
pub use lowering::{BackendFailure, Unsupported};
pub(crate) use lowering::{
    BaselinePlan, PACKED_REGISTER_LANES, decode_register_list, pack_register_lanes, reg_offset,
    unpack_register_lanes,
};
use runtime_ops::*;
pub(crate) use runtime_ops::{PropertySourceCell, jit_backedge_poll_stub};
pub(crate) use value_abi::*;

/// GC header type tag for an ordinary `ObjectBody` (mirrors
/// `otter_vm::object::OBJECT_BODY_TYPE_TAG`). A heap cell is disambiguated by
/// this tag before an inline shape-slot read, since every cell value word is a
/// bare cage offset with no class tag of its own.
pub(crate) const OBJECT_BODY_TYPE_TAG: u32 = 0x11;

/// Hook-lifetime resolution of the JIT-owned transition inventory.
///
/// Built once when the compiler hook is constructed and reused by every
/// compilation: entries are indexed by descriptor id and validated against
/// the descriptor's signature family, so a compile bakes addresses through
/// one O(1) lookup instead of re-resolving the binding inventory.
pub struct TransitionTable {
    /// `(entry_addr, signature)` indexed by `descriptor.id - 1`; VM-owned and
    /// statically typed slots stay vacant.
    entries: Box<[(u64, Option<otter_vm::native_abi::RuntimeStubSignature>)]>,
}

impl Default for TransitionTable {
    fn default() -> Self {
        Self::resolve()
    }
}

impl TransitionTable {
    /// Resolve and validate the complete active JIT-owned binding inventory.
    #[must_use]
    pub fn resolve() -> Self {
        let descriptors = otter_vm::native_abi::RUNTIME_STUB_DESCRIPTORS;
        let mut entries = vec![(0u64, None); descriptors.len()].into_boxed_slice();
        for binding in runtime_stub_bindings() {
            let descriptor = descriptors[binding.id as usize - 1];
            assert_eq!(descriptor.id, binding.id);
            assert_eq!(descriptor.signature, binding.signature);
            assert_ne!(binding.entry_addr, 0);
            entries[binding.id as usize - 1] = (binding.entry_addr as u64, Some(binding.signature));
        }
        Self { entries }
    }

    /// Validated machine entry for `descriptor`.
    ///
    /// Panics on an unbound id or a signature-family mismatch: both are
    /// compiler-construction bugs, not runtime conditions.
    pub(crate) fn entry(&self, descriptor: otter_vm::native_abi::RuntimeStubDescriptor) -> u64 {
        let (addr, signature) = self.entries[descriptor.id as usize - 1];
        assert_eq!(
            signature,
            Some(descriptor.signature),
            "runtime stub {} has no JIT binding for its signature family",
            descriptor.id
        );
        addr
    }

    /// Validated machine entry for a status-reporting `Variadic` transition.
    pub(crate) fn variadic_entry(
        &self,
        descriptor: otter_vm::native_abi::RuntimeStubDescriptor,
    ) -> u64 {
        assert_eq!(
            descriptor.signature,
            otter_vm::native_abi::RuntimeStubSignature::Variadic
        );
        self.entry(descriptor)
    }

    /// Swap any stub entry for a test double, whatever its signature family.
    #[cfg(test)]
    pub(crate) fn replace_entry_for_test(
        &mut self,
        descriptor: otter_vm::native_abi::RuntimeStubDescriptor,
        entry_addr: usize,
    ) {
        let slot = &mut self.entries[descriptor.id as usize - 1];
        *slot = (entry_addr as u64, Some(descriptor.signature));
    }
}

/// JIT-owned runtime transitions installed into the isolate entry table at
/// compiler-hook install. Each binding names its VM descriptor id and
/// signature family; the VM validates the pairing before installation and
/// rejects an installation that leaves any active inventory slot vacant, so
/// this table is the complete machine inventory of callable transition entries.
pub(crate) fn runtime_stub_bindings() -> Vec<otter_vm::JitRuntimeStubBinding> {
    use otter_vm::native_abi as abi;
    let binding = |descriptor: abi::RuntimeStubDescriptor,
                   entry_addr: usize|
     -> otter_vm::JitRuntimeStubBinding {
        otter_vm::JitRuntimeStubBinding {
            id: descriptor.id,
            signature: descriptor.signature,
            entry_addr,
        }
    };
    macro_rules! context_words_binding {
        ($descriptor:expr, $entry:path, 1) => {{
            const _: () = assert!(matches!(
                $descriptor.signature,
                abi::RuntimeStubSignature::ContextWords
            ));
            const _: () = assert!($descriptor.argument_count == 1);
            let typed: extern "C" fn(*mut JitCtx, u64) -> otter_vm::native_abi::NativeResultPair =
                $entry;
            binding($descriptor, typed as *const () as usize)
        }};
        ($descriptor:expr, $entry:path, 2) => {{
            const _: () = assert!(matches!(
                $descriptor.signature,
                abi::RuntimeStubSignature::ContextWords
            ));
            const _: () = assert!($descriptor.argument_count == 2);
            let typed: extern "C" fn(
                *mut JitCtx,
                u64,
                u64,
            ) -> otter_vm::native_abi::NativeResultPair = $entry;
            binding($descriptor, typed as *const () as usize)
        }};
        ($descriptor:expr, $entry:path, 3) => {{
            const _: () = assert!(matches!(
                $descriptor.signature,
                abi::RuntimeStubSignature::ContextWords
            ));
            const _: () = assert!($descriptor.argument_count == 3);
            let typed: extern "C" fn(
                *mut JitCtx,
                u64,
                u64,
                u64,
            ) -> otter_vm::native_abi::NativeResultPair = $entry;
            binding($descriptor, typed as *const () as usize)
        }};
        ($descriptor:expr, $entry:path, 4) => {{
            const _: () = assert!(matches!(
                $descriptor.signature,
                abi::RuntimeStubSignature::ContextWords
            ));
            const _: () = assert!($descriptor.argument_count == 4);
            let typed: extern "C" fn(
                *mut JitCtx,
                u64,
                u64,
                u64,
                u64,
            ) -> otter_vm::native_abi::NativeResultPair = $entry;
            binding($descriptor, typed as *const () as usize)
        }};
    }
    macro_rules! committed_value2_binding {
        ($descriptor:expr, $entry:path) => {{
            const _: () = assert!(matches!(
                $descriptor.signature,
                abi::RuntimeStubSignature::CommittedValue2
            ));
            const _: () = assert!($descriptor.argument_count == 2);
            let typed: extern "C" fn(
                *mut JitCtx,
                u64,
                u64,
            ) -> otter_vm::native_abi::NativeResultPair = $entry;
            binding($descriptor, typed as *const () as usize)
        }};
    }
    vec![
        binding(
            abi::STUB_JIT_BACKEDGE_POLL,
            jit_backedge_poll_stub as *const () as usize,
        ),
        binding(abi::STUB_JIT_ADD, jit_add_stub as *const () as usize),
        binding(
            abi::STUB_JIT_LOAD_ELEMENT,
            jit_load_element_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STORE_ELEMENT,
            jit_store_element_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_DEFINE_OWN_PROPERTY,
            jit_define_own_property_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_DEOPT_STACK_CALL,
            jit_deopt_stack_call_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_PROPERTY,
            jit_load_property_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STORE_PROPERTY,
            jit_store_property_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_CALL_METHOD_VALUE,
            jit_call_method_value_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_CALL_WITH_THIS_VALUE,
            jit_call_with_this_value_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_CONSTRUCT_VALUE,
            jit_construct_value_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_ROUTE_THROW,
            jit_route_throw_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW,
            jit_acknowledge_caught_throw_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_DEFINE_DATA_PROPERTY,
            jit_define_data_property_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_BUILTIN_ERROR,
            jit_load_builtin_error_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_MAKE_FN,
            jit_make_fn_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_MAKE_CLOSURE,
            jit_make_closure_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_NEW_OBJECT,
            jit_new_object_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_COLLECT_ARGUMENTS,
            jit_collect_arguments_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FORWARD_SOURCE_READY,
            jit_forward_source_ready_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_CALL_FORWARD_ARGUMENTS,
            jit_call_forward_arguments_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_NEW_ARRAY,
            jit_new_array_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FRESH_UPVALUE,
            jit_fresh_upvalue_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_PUSH_NATIVE_ACTIVATION,
            jit_push_native_activation_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_POP_NATIVE_ACTIVATION,
            jit_pop_native_activation_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_INLINE_CLOSURE_UPVALUES,
            jit_inline_closure_upvalues_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOOSE_EQ,
            jit_loose_eq_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_REGEXP,
            jit_load_regexp_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_CONSTRUCT,
            jit_construct_stub as *const () as usize,
        ),
        context_words_binding!(
            abi::STUB_JIT_PREPARE_BASE_CONSTRUCT,
            jit_prepare_base_construct_stub,
            3
        ),
        context_words_binding!(
            abi::STUB_JIT_TRY_PREPARE_BASE_CONSTRUCT,
            jit_try_prepare_base_construct_stub,
            4
        ),
        context_words_binding!(
            abi::STUB_JIT_DERIVED_CONSTRUCT_RESULT,
            jit_derived_construct_result_stub,
            2
        ),
        binding(
            abi::STUB_JIT_CLASS_SUPER_CONSTRUCTOR,
            jit_class_super_constructor_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FORWARD_ARGUMENT_COUNT,
            jit_forward_argument_count_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FORWARD_CALL_PLAN,
            jit_forward_call_plan_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_COPY_FORWARDED_ARGUMENTS,
            jit_copy_forwarded_arguments_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_COPY_SPREAD_ARGUMENTS,
            jit_copy_spread_arguments_stub as *const () as usize,
        ),
        context_words_binding!(
            abi::STUB_JIT_INITIALIZE_UPVALUES,
            jit_initialize_upvalues_stub,
            3
        ),
        binding(
            abi::STUB_JIT_COERCE_UNARY,
            jit_coerce_unary_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_NUMERIC_OP,
            jit_numeric_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_EXCEPTION_OP,
            jit_exception_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_ITERATOR_OP,
            jit_iterator_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_BIND_FUNCTION,
            jit_bind_function_stub as *const () as usize,
        ),
        committed_value2_binding!(abi::STUB_JIT_BINDING_VALUE, jit_binding_value_stub),
        committed_value2_binding!(
            abi::STUB_JIT_GLOBAL_DECLARATION_VALUE,
            jit_global_declaration_value_stub
        ),
        binding(
            abi::STUB_JIT_OBJECT_PROTOCOL_VALUE,
            jit_object_protocol_value_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_DELETE_OP,
            jit_delete_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_SCALAR_VALUE,
            jit_scalar_value_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_SUPER_OP,
            jit_super_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_PRIVATE_OP,
            jit_private_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_VALUE_LOAD_OP,
            jit_value_load_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_CONSTRUCT_OP,
            jit_construct_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STRUCTURAL_OP,
            jit_structural_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_CLASS_OP,
            jit_class_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_VARIADIC_OP,
            jit_variadic_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STATIC_CALL_OP,
            jit_static_call_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_SPREAD_CALL_OP,
            jit_spread_call_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_CLASS_VALUE_OP,
            jit_class_value_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_MODULE_OP,
            jit_module_op_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_RESOLVE_DIRECT_ENTRY,
            jit_resolve_direct_entry_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FINISH_ERROR,
            jit_finish_error_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_DEOPT_WRITEBACK,
            jit_deopt_writeback_stub as *const () as usize,
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Lowering-plan and stub-inventory contract tests. Machine-code execution
    //! coverage lives in [`crate::template`]'s test suite.

    use super::{BaselinePlan, Unsupported};
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, jit::JitTestInstruction};

    const STRIDE: u32 = 4;

    fn view(instrs: &[(Op, Vec<Operand>)]) -> JitCompileSnapshot {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, operands))| {
                JitTestInstruction::new(*op, idx as u32, idx as u32 * STRIDE, operands.clone())
            })
            .collect();
        JitCompileSnapshot::without_feedback(0, 1, 8, instructions)
    }

    #[test]
    fn transition_bindings_cover_the_descriptor_inventory() {
        use otter_vm::native_abi::{RUNTIME_STUB_DESCRIPTORS, RuntimeStubSignature};
        let bindings = super::runtime_stub_bindings();
        let mut seen = std::collections::BTreeSet::new();
        for binding in &bindings {
            assert!(seen.insert(binding.id), "duplicate binding {}", binding.id);
            let descriptor = RUNTIME_STUB_DESCRIPTORS[binding.id as usize - 1];
            assert_eq!(descriptor.id, binding.id);
            assert_eq!(descriptor.signature, binding.signature);
            assert_ne!(binding.entry_addr, 0);
        }
        // Exactly the active JIT-owned slots.
        let jit_owned = RUNTIME_STUB_DESCRIPTORS
            .iter()
            .filter(|descriptor| {
                !matches!(
                    descriptor.signature,
                    RuntimeStubSignature::LeafValue2
                        | RuntimeStubSignature::Float64Leaf2
                        | RuntimeStubSignature::Float64ToWordLeaf1
                        | RuntimeStubSignature::MutatingLeafValue2
                        | RuntimeStubSignature::MutatingLeafValue3
                        | RuntimeStubSignature::AllocValue3
                )
            })
            .count();
        assert_eq!(bindings.len(), jit_owned);
    }

    #[test]
    fn lowering_plan_rejects_non_boundary_branch_target() {
        let v = view(&[
            (Op::Jump, vec![Operand::Imm32(8)]),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert_eq!(
            BaselinePlan::build(&v).err(),
            Some(Unsupported::BranchTarget(9))
        );
    }

    #[test]
    fn lowering_plan_publishes_canonical_branch_target() {
        let v = view(&[
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::ReturnUndefined, vec![]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");
        assert_eq!(
            plan.instructions[0]
                .branch_operands()
                .map(|operands| operands.target),
            Ok(1)
        );
    }

    #[test]
    fn lowering_plan_assigns_allocating_add_safepoint() {
        let v = view(&[
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");
        let id = plan
            .add_alloc_safepoints
            .get(&0)
            .copied()
            .expect("add safepoint");
        assert!(plan.safepoint_records.iter().any(|record| record.id == id));
    }

    #[test]
    fn lowering_plan_assigns_typed_array_construct_safepoints() {
        let v = view(&[
            (
                Op::ArrayConstruct,
                vec![Operand::Register(0), Operand::ConstIndex(0)],
            ),
            (
                Op::ArrayConstruct,
                vec![
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::ArrayConstruct,
                vec![
                    Operand::Register(3),
                    Operand::ConstIndex(2),
                    Operand::Register(4),
                    Operand::Register(5),
                ],
            ),
            (Op::ReturnUndefined, vec![]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");
        assert_eq!(plan.array_construct_alloc_safepoints.len(), 2);
        assert!(
            plan.array_construct_alloc_safepoints
                .contains_key(&plan.instructions[0].byte_pc)
        );
        assert!(
            plan.array_construct_alloc_safepoints
                .contains_key(&plan.instructions[1].byte_pc)
        );
        assert!(
            !plan
                .array_construct_alloc_safepoints
                .contains_key(&plan.instructions[2].byte_pc)
        );
        for id in plan.array_construct_alloc_safepoints.values() {
            assert!(plan.safepoint_records.iter().any(|record| record.id == *id));
        }
    }

    #[test]
    fn lowering_plan_publishes_typed_fixed_operands() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(0), Operand::Imm32(42)],
            ),
            (
                Op::LoadString,
                vec![Operand::Register(1), Operand::ConstIndex(0)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::Neg, vec![Operand::Register(3), Operand::Register(2)]),
            (
                Op::StoreLocal,
                vec![Operand::Register(3), Operand::Imm32(7)],
            ),
            (
                Op::LoadElement,
                vec![
                    Operand::Register(4),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::StoreElement,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(3),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");

        assert_eq!(plan.instructions.len(), v.instructions.len());
        assert_eq!(plan.instructions[0].op, Op::LoadInt32);
        assert_eq!(plan.instructions[0].byte_pc, v.instructions[0].byte_pc);
        let load = plan.instructions[0]
            .load_int32_operands()
            .expect("LoadInt32 operands");
        assert_eq!((load.dst, load.value), (0, 42));
        let string = plan.instructions[1]
            .constant_operands()
            .expect("LoadString operands");
        assert_eq!((string.dst, string.constant), (1, 0));
        let add = plan.instructions[2]
            .binary_operands()
            .expect("Add operands");
        assert_eq!((add.dst, add.lhs, add.rhs), (2, 0, 1));
        let neg = plan.instructions[3].unary_operands().expect("Neg operands");
        assert_eq!((neg.dst, neg.src), (3, 2));
        let store = plan.instructions[4]
            .local_operands()
            .expect("StoreLocal operands");
        assert_eq!((store.value, store.local), (3, 7));
        let load_element = plan.instructions[5]
            .element_load_operands()
            .expect("LoadElement operands");
        assert_eq!(
            (load_element.dst, load_element.receiver, load_element.index),
            (4, 0, 1)
        );
        let store_element = plan.instructions[6]
            .element_store_operands()
            .expect("StoreElement operands");
        assert_eq!(
            (
                store_element.receiver,
                store_element.index,
                store_element.value
            ),
            (0, 1, 3)
        );
        assert_eq!(
            plan.instructions[7]
                .source_operands()
                .map(|operands| operands.src),
            Ok(3)
        );
    }

    #[test]
    fn lowering_plan_preserves_to_primitive_hint() {
        let v = view(&[
            (
                Op::ToPrimitive,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(7),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");
        let operands = plan.instructions[0]
            .to_primitive_operands()
            .expect("ToPrimitive operands");
        assert_eq!((operands.dst, operands.src, operands.hint), (1, 0, 7));
    }

    #[test]
    fn lowering_plan_publishes_property_and_schema_binding_operands() {
        let v = view(&[
            (
                Op::LoadProperty,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(7),
                ],
            ),
            (
                Op::StoreProperty,
                vec![
                    Operand::Register(0),
                    Operand::ConstIndex(7),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
            (
                Op::LoadUpvalue,
                vec![Operand::Register(4), Operand::Imm32(5)],
            ),
            (
                Op::StoreUpvalueChecked,
                vec![Operand::Register(4), Operand::Imm32(5)],
            ),
            (
                Op::StoreGlobalChecked,
                vec![
                    Operand::Register(4),
                    Operand::ConstIndex(7),
                    Operand::Register(5),
                ],
            ),
            (
                Op::DefineGlobalVar,
                vec![Operand::ConstIndex(7), Operand::Register(4)],
            ),
            (Op::ReturnValue, vec![Operand::Register(4)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");

        let load = plan.instructions[0]
            .property_load_operands()
            .expect("LoadProperty operands");
        assert_eq!((load.dst, load.object, load.name), (2, 0, 7));
        let store = plan.instructions[1]
            .property_store_operands()
            .expect("StoreProperty operands");
        assert_eq!(
            (store.object, store.name, store.value, store.scratch),
            (0, 7, 2, 3)
        );
        let load_upvalue = plan.instructions[2]
            .binding_value_operands()
            .expect("LoadUpvalue binding operands");
        assert_eq!(load_upvalue.result, Some(4));
        assert_eq!(load_upvalue.values, [None, None]);
        let store_upvalue = plan.instructions[3]
            .binding_value_operands()
            .expect("StoreUpvalueChecked binding operands");
        assert_eq!(store_upvalue.result, None);
        assert_eq!(store_upvalue.values, [Some(4), None]);
        let store_global = plan.instructions[4]
            .binding_value_operands()
            .expect("StoreGlobalChecked binding operands");
        assert_eq!(store_global.result, None);
        assert_eq!(store_global.values, [Some(4), Some(5)]);
        let declaration = plan.instructions[5]
            .global_declaration_operands()
            .expect("DefineGlobalVar declaration operands");
        assert_eq!(declaration.values, [Some(4), None]);
    }

    #[test]
    fn lowering_plan_owns_variadic_operand_tails() {
        let v = view(&[
            (
                Op::NewArray,
                vec![
                    Operand::Register(0),
                    Operand::ConstIndex(2),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::MakeClosure,
                vec![
                    Operand::Register(4),
                    Operand::ConstIndex(9),
                    Operand::ConstIndex(2),
                    Operand::Imm32(0),
                    Operand::Imm32(1),
                ],
            ),
            (Op::FreshUpvalue, vec![Operand::Imm32(6)]),
            (
                Op::DefineDataProperty,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");

        let array = plan.instructions[0]
            .new_array_operands()
            .expect("NewArray operands");
        assert_eq!(array.dst, 0);
        assert_eq!(plan.register_tail(array.elements), Ok(&[1, 2][..]));
        let closure = plan.instructions[1]
            .make_closure_operands()
            .expect("MakeClosure operands");
        assert_eq!((closure.dst, closure.function), (4, 9));
        assert_eq!(plan.index_tail(closure.parents), Ok(&[0, 1][..]));
        assert_eq!(
            plan.instructions[2]
                .immediate_operands()
                .map(|operands| operands.value),
            Ok(6)
        );
        let triple = plan.instructions[3]
            .triple_operands()
            .expect("DefineDataProperty operands");
        assert_eq!((triple.first, triple.second, triple.third), (0, 1, 2));
    }

    #[test]
    fn lowering_plan_owns_call_operand_tails() {
        let v = view(&[
            (
                Op::Call,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(2),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
            (
                Op::CallMethodValue,
                vec![
                    Operand::Register(4),
                    Operand::Register(1),
                    Operand::ConstIndex(7),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");

        let call = plan.instructions[0].call_operands().expect("Call operands");
        assert_eq!((call.dst, call.callee), (0, 1));
        assert_eq!(plan.register_tail(call.arguments), Ok(&[2, 3][..]));

        let method = plan.instructions[1]
            .method_call_operands()
            .expect("CallMethodValue operands");
        assert_eq!((method.dst, method.receiver, method.name), (4, 1, 7));
        assert_eq!(plan.register_tail(method.arguments), Ok(&[2][..]));
    }
}
