//! Shared native-compilation infrastructure for the template compiler.
//!
//! Owns everything the machine backend consumes that is not itself a code
//! template: the frozen compiled-entry ABI (`JitCtx`/`JitRet` and their field
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
//! - [`runtime_stub_bindings`] — the complete JIT-owned transition inventory.
//! - [`TransitionTable`] — hook-lifetime descriptor-id-indexed resolution of
//!   that inventory for O(1) compile-time address baking.
//!
//! # Invariants
//! - Compiled functions are `extern "C" fn(*mut JitCtx) -> JitRet`. A normal
//!   return yields `status: 0`; a failed guard yields `status: 1` (the VM
//!   resumes on the interpreter at the exact published PC); a re-entered VM
//!   call that threw yields `status: 2` with the error parked in `ctx.error`.
//! - Interpreter-visible registers remain in the published frame window at
//!   every side exit and allocating/reentrant call; no movable JS pointer is
//!   kept only in a machine register across a safepoint.
//! - Runtime entries install through validated descriptor bindings; no slot
//!   may stay vacant with a hook present, so the emitted and audited stub
//!   sets cannot drift.
//!
//! # See also
//! - [`crate::template`] — the production compiler consuming this module.
//! - `JIT_DESIGN.md` §3.2 (backend), §3.5 (GC contract).

mod abi;
mod code;
mod lowering;
mod runtime_ops;
mod value_abi;
pub(crate) use abi::*;
pub(crate) use code::enter_compiled;
pub use lowering::Unsupported;
pub(crate) use lowering::{
    BaselinePlan, MAX_METHOD_ARGS, pack_method_arg_regs, reg_offset, unpack_method_arg_regs,
};
use runtime_ops::*;
pub(crate) use runtime_ops::{IC_WAYS, WhiskerIcCell, jit_backedge_poll_stub};
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
    /// `(entry_addr, signature)` indexed by `descriptor.id - 1`; VM-owned
    /// slots (resolved through their own compile-time accessors) stay vacant.
    entries: Box<[(u64, Option<otter_vm::native_abi::RuntimeStubSignature>)]>,
}

impl Default for TransitionTable {
    fn default() -> Self {
        Self::resolve()
    }
}

impl TransitionTable {
    /// Resolve and validate the complete JIT-owned binding inventory.
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

    /// Validated machine entry for a `NullaryValue` producer.
    pub(crate) fn nullary_value_entry(
        &self,
        descriptor: otter_vm::native_abi::RuntimeStubDescriptor,
    ) -> u64 {
        assert_eq!(
            descriptor.signature,
            otter_vm::native_abi::RuntimeStubSignature::NullaryValue
        );
        self.entry(descriptor)
    }
}

/// JIT-owned runtime transitions installed into the isolate entry table at
/// compiler-hook install. Each binding names its VM descriptor id and
/// signature family; the VM validates the pairing before installation and
/// rejects an installation that leaves any inventory slot vacant, so this
/// table is the complete machine inventory of transition entries.
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
    vec![
        binding(
            abi::STUB_JIT_BACKEDGE_POLL,
            jit_backedge_poll_stub as *const () as usize,
        ),
        binding(abi::STUB_JIT_ADD, jit_add_stub as *const () as usize),
        binding(abi::STUB_JIT_NEG, jit_neg_stub as *const () as usize),
        binding(
            abi::STUB_JIT_MATH_CALL,
            jit_math_call_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_GLOBAL,
            jit_load_global_stub as *const () as usize,
        ),
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
            abi::STUB_JIT_COLLECTION_METHOD_IC,
            jit_call_collection_method_ic_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FINISH_DIRECT_CALL_BAILED,
            jit_finish_direct_call_bailed_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_SELF_CALL_BAIL,
            jit_self_call_bail_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_PROP_WINDOW,
            jit_load_prop_window_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STORE_PROP_WINDOW,
            jit_store_prop_window_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_DEFINE_DATA_PROPERTY,
            jit_define_data_property_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_STRING,
            jit_load_string_stub as *const () as usize,
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
            abi::STUB_JIT_NEW_ARRAY,
            jit_new_array_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FRESH_UPVALUE,
            jit_fresh_upvalue_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_PREPARE_DIRECT_CALL,
            jit_prepare_direct_call_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_PREPARE_DIRECT_METHOD_CALL,
            jit_prepare_direct_method_call_stub as *const () as usize,
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
            abi::STUB_JIT_ABORT_DIRECT_CALL,
            jit_abort_direct_call_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FINISH_DIRECT_CALL_RETURNED,
            jit_finish_direct_call_returned_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_UPVALUE,
            jit_load_upvalue_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STORE_UPVALUE,
            jit_store_upvalue_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STORE_UPVALUE_CHECKED,
            jit_store_upvalue_checked_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_WRITE_BARRIER,
            jit_write_barrier_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_WRITE_BARRIER_WINDOW,
            jit_write_barrier_window_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_INLINE_CLOSURE_UPVALUES,
            jit_inline_closure_upvalues_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_MATH_RANDOM,
            otter_jit_math_random as *const () as usize,
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
        // Exactly the JIT-owned slots (everything the VM phase leaves vacant).
        let jit_owned = RUNTIME_STUB_DESCRIPTORS
            .iter()
            .filter(|descriptor| {
                !matches!(
                    descriptor.signature,
                    RuntimeStubSignature::LeafValue2 | RuntimeStubSignature::AllocValue3
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
                    Operand::Register(5),
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
                store_element.value,
                store_element.scratch
            ),
            (0, 1, 3, 5)
        );
        assert_eq!(
            plan.instructions[7]
                .source_operands()
                .map(|operands| operands.src),
            Ok(3)
        );
    }

    #[test]
    fn lowering_plan_publishes_property_and_upvalue_operands() {
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
            .upvalue_operands()
            .expect("LoadUpvalue operands");
        assert_eq!((load_upvalue.value, load_upvalue.index), (4, 5));
        let store_upvalue = plan.instructions[3]
            .upvalue_operands()
            .expect("StoreUpvalueChecked operands");
        assert_eq!((store_upvalue.value, store_upvalue.index), (4, 5));
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
                Op::MathCall,
                vec![
                    Operand::Register(3),
                    Operand::ConstIndex(0),
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
        let math = plan.instructions[1]
            .math_call_operands()
            .expect("MathCall operands");
        assert_eq!((math.dst, math.method), (3, 0));
        assert_eq!(plan.register_tail(math.arguments), Ok(&[1, 2][..]));
        let closure = plan.instructions[2]
            .make_closure_operands()
            .expect("MakeClosure operands");
        assert_eq!((closure.dst, closure.function), (4, 9));
        assert_eq!(plan.index_tail(closure.parents), Ok(&[0, 1][..]));
        assert_eq!(
            plan.instructions[3]
                .immediate_operands()
                .map(|operands| operands.value),
            Ok(6)
        );
        let triple = plan.instructions[4]
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
