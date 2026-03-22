//! Human-readable MIR pretty-printer.
//!
//! Used for `OTTER_JIT_DUMP_MIR=1` diagnostics and debugging.

use std::fmt;

use super::graph::{MirGraph, MirInstr, ValueId};
use super::nodes::MirOp;

impl fmt::Display for MirGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "function {} (locals={}, regs={}, params={})",
            self.function_name, self.local_count, self.register_count, self.param_count
        )?;

        for block in &self.blocks {
            writeln!(f)?;
            write!(f, "{}:", block.id)?;
            if !block.predecessors.is_empty() {
                write!(f, "  ; preds =")?;
                for pred in &block.predecessors {
                    write!(f, " {}", pred)?;
                }
            }
            writeln!(f)?;

            for instr in &block.instrs {
                write!(f, "  ")?;
                format_instr(f, instr)?;
                writeln!(f)?;
            }
        }

        if !self.deopts.is_empty() {
            writeln!(f)?;
            writeln!(f, "; deopt table ({} entries)", self.deopts.len())?;
            for (i, deopt) in self.deopts.iter().enumerate() {
                write!(f, ";   deopt{} -> pc{}", i, deopt.bytecode_pc)?;
                if !deopt.live_state.is_empty() {
                    write!(f, " [")?;
                    for (j, live) in deopt.live_state.iter().enumerate() {
                        if j > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{:?}{}={}", live.kind, live.index, live.value)?;
                    }
                    write!(f, "]")?;
                }
                writeln!(f)?;
            }
        }

        Ok(())
    }
}

fn format_instr(f: &mut fmt::Formatter<'_>, instr: &MirInstr) -> fmt::Result {
    let v = instr.value;
    let ty = instr.ty;

    if ty != super::types::MirType::Void {
        write!(f, "{}: {} = ", v, ty)?;
    }

    match &instr.op {
        // Constants
        MirOp::Const(bits) => write!(f, "const 0x{:016x}", bits),
        MirOp::Undefined => write!(f, "undefined"),
        MirOp::Null => write!(f, "null"),
        MirOp::True => write!(f, "true"),
        MirOp::False => write!(f, "false"),
        MirOp::ConstInt32(n) => write!(f, "const.i32 {}", n),
        MirOp::ConstFloat64(n) => write!(f, "const.f64 {}", n),

        // Guards
        MirOp::GuardInt32 { val, deopt } => write!(f, "guard_int32 {} {}", val, deopt),
        MirOp::GuardFloat64 { val, deopt } => write!(f, "guard_f64 {} {}", val, deopt),
        MirOp::GuardObject { val, deopt } => write!(f, "guard_object {} {}", val, deopt),
        MirOp::GuardString { val, deopt } => write!(f, "guard_string {} {}", val, deopt),
        MirOp::GuardFunction { val, deopt } => write!(f, "guard_function {} {}", val, deopt),
        MirOp::GuardBool { val, deopt } => write!(f, "guard_bool {} {}", val, deopt),
        MirOp::GuardShape {
            obj,
            shape_id,
            deopt,
        } => {
            write!(f, "guard_shape {} shape=0x{:x} {}", obj, shape_id, deopt)
        }
        MirOp::GuardProtoEpoch { epoch, deopt } => {
            write!(f, "guard_proto_epoch epoch={} {}", epoch, deopt)
        }
        MirOp::GuardArrayDense { obj, deopt } => write!(f, "guard_array_dense {} {}", obj, deopt),
        MirOp::GuardBoundsCheck { arr, idx, deopt } => {
            write!(f, "guard_bounds {} {} {}", arr, idx, deopt)
        }
        MirOp::GuardNotHole { val, deopt } => write!(f, "guard_not_hole {} {}", val, deopt),

        // Boxing
        MirOp::BoxInt32(v) => write!(f, "box_i32 {}", v),
        MirOp::BoxFloat64(v) => write!(f, "box_f64 {}", v),
        MirOp::BoxBool(v) => write!(f, "box_bool {}", v),
        MirOp::UnboxInt32(v) => write!(f, "unbox_i32 {}", v),
        MirOp::UnboxFloat64(v) => write!(f, "unbox_f64 {}", v),
        MirOp::Int32ToFloat64(v) => write!(f, "i32_to_f64 {}", v),

        // Arithmetic
        MirOp::AddI32 { lhs, rhs, deopt } => write!(f, "add.i32 {} {} {}", lhs, rhs, deopt),
        MirOp::SubI32 { lhs, rhs, deopt } => write!(f, "sub.i32 {} {} {}", lhs, rhs, deopt),
        MirOp::MulI32 { lhs, rhs, deopt } => write!(f, "mul.i32 {} {} {}", lhs, rhs, deopt),
        MirOp::DivI32 { lhs, rhs, deopt } => write!(f, "div.i32 {} {} {}", lhs, rhs, deopt),
        MirOp::IncI32 { val, deopt } => write!(f, "inc.i32 {} {}", val, deopt),
        MirOp::DecI32 { val, deopt } => write!(f, "dec.i32 {} {}", val, deopt),
        MirOp::NegI32 { val, deopt } => write!(f, "neg.i32 {} {}", val, deopt),
        MirOp::AddF64 { lhs, rhs } => write!(f, "add.f64 {} {}", lhs, rhs),
        MirOp::SubF64 { lhs, rhs } => write!(f, "sub.f64 {} {}", lhs, rhs),
        MirOp::MulF64 { lhs, rhs } => write!(f, "mul.f64 {} {}", lhs, rhs),
        MirOp::DivF64 { lhs, rhs } => write!(f, "div.f64 {} {}", lhs, rhs),
        MirOp::ModF64 { lhs, rhs } => write!(f, "mod.f64 {} {}", lhs, rhs),
        MirOp::NegF64(v) => write!(f, "neg.f64 {}", v),

        // Bitwise
        MirOp::BitAnd { lhs, rhs } => write!(f, "bit_and {} {}", lhs, rhs),
        MirOp::BitOr { lhs, rhs } => write!(f, "bit_or {} {}", lhs, rhs),
        MirOp::BitXor { lhs, rhs } => write!(f, "bit_xor {} {}", lhs, rhs),
        MirOp::Shl { lhs, rhs } => write!(f, "shl {} {}", lhs, rhs),
        MirOp::Shr { lhs, rhs } => write!(f, "shr {} {}", lhs, rhs),
        MirOp::Ushr { lhs, rhs } => write!(f, "ushr {} {}", lhs, rhs),
        MirOp::BitNot(v) => write!(f, "bit_not {}", v),

        // Comparisons
        MirOp::CmpI32 { op, lhs, rhs } => write!(f, "cmp.i32 {} {} {}", op, lhs, rhs),
        MirOp::CmpF64 { op, lhs, rhs } => write!(f, "cmp.f64 {} {} {}", op, lhs, rhs),
        MirOp::CmpStrictEq { lhs, rhs } => write!(f, "strict_eq {} {}", lhs, rhs),
        MirOp::CmpStrictNe { lhs, rhs } => write!(f, "strict_ne {} {}", lhs, rhs),
        MirOp::LogicalNot(v) => write!(f, "not {}", v),
        MirOp::IsTruthy(v) => write!(f, "is_truthy {}", v),

        // Property access
        MirOp::GetPropShaped {
            obj,
            offset,
            inline,
        } => {
            write!(
                f,
                "get_prop_shaped {} offset={} inline={}",
                obj, offset, inline
            )
        }
        MirOp::SetPropShaped {
            obj,
            offset,
            val,
            inline,
        } => {
            write!(
                f,
                "set_prop_shaped {} offset={} {} inline={}",
                obj, offset, val, inline
            )
        }
        MirOp::GetPropGeneric { obj, key, ic_index } => {
            write!(f, "get_prop {} {} ic={}", obj, key, ic_index)
        }
        MirOp::SetPropGeneric {
            obj,
            key,
            val,
            ic_index,
        } => {
            write!(f, "set_prop {} {} {} ic={}", obj, key, val, ic_index)
        }
        MirOp::GetPropConstGeneric {
            obj,
            name_idx,
            ic_index,
        } => {
            write!(
                f,
                "get_prop_const {} name={} ic={}",
                obj, name_idx, ic_index
            )
        }
        MirOp::SetPropConstGeneric {
            obj,
            name_idx,
            val,
            ic_index,
        } => {
            write!(
                f,
                "set_prop_const {} name={} {} ic={}",
                obj, name_idx, val, ic_index
            )
        }
        MirOp::DeleteProp { obj, key } => write!(f, "delete_prop {} {}", obj, key),

        // Array access
        MirOp::GetElemDense { arr, idx } => write!(f, "get_elem_dense {} {}", arr, idx),
        MirOp::SetElemDense { arr, idx, val } => {
            write!(f, "set_elem_dense {} {} {}", arr, idx, val)
        }
        MirOp::ArrayLength(arr) => write!(f, "array_length {}", arr),
        MirOp::ArrayPush { arr, val } => write!(f, "array_push {} {}", arr, val),
        MirOp::GetElemGeneric { obj, key, ic_index } => {
            write!(f, "get_elem {} {} ic={}", obj, key, ic_index)
        }
        MirOp::SetElemGeneric {
            obj,
            key,
            val,
            ic_index,
        } => {
            write!(f, "set_elem {} {} {} ic={}", obj, key, val, ic_index)
        }

        // Calls
        MirOp::CallDirect { target, args } => {
            write!(f, "call_direct {} ({})", target, format_args_list(args))
        }
        MirOp::CallMonomorphic {
            callee,
            expected_bits,
            args,
            deopt,
        } => {
            write!(
                f,
                "call_mono {} expect=0x{:x} ({}) {}",
                callee,
                expected_bits,
                format_args_list(args),
                deopt,
            )
        }
        MirOp::CallGeneric {
            callee,
            args,
            ic_index,
        } => {
            write!(
                f,
                "call {} ({}) ic={}",
                callee,
                format_args_list(args),
                ic_index
            )
        }
        MirOp::CallMethodGeneric {
            obj,
            name_idx,
            args,
            ic_index,
        } => {
            write!(
                f,
                "call_method {} name={} ({}) ic={}",
                obj,
                name_idx,
                format_args_list(args),
                ic_index,
            )
        }
        MirOp::ConstructGeneric { callee, args } => {
            write!(f, "construct {} ({})", callee, format_args_list(args))
        }

        // Variables
        MirOp::LoadLocal(idx) => write!(f, "load_local {}", idx),
        MirOp::StoreLocal { idx, val } => write!(f, "store_local {} {}", idx, val),
        MirOp::LoadRegister(idx) => write!(f, "load_reg {}", idx),
        MirOp::StoreRegister { idx, val } => write!(f, "store_reg {} {}", idx, val),
        MirOp::LoadUpvalue(idx) => write!(f, "load_upvalue {}", idx),
        MirOp::StoreUpvalue { idx, val } => write!(f, "store_upvalue {} {}", idx, val),
        MirOp::CloseUpvalue(idx) => write!(f, "close_upvalue {}", idx),
        MirOp::LoadThis => write!(f, "load_this"),

        // Globals
        MirOp::GetGlobal { name_idx, ic_index } => {
            write!(f, "get_global name={} ic={}", name_idx, ic_index)
        }
        MirOp::SetGlobal {
            name_idx,
            val,
            ic_index,
        } => {
            write!(f, "set_global name={} {} ic={}", name_idx, val, ic_index)
        }

        // Object/Array creation
        MirOp::NewObject => write!(f, "new_object"),
        MirOp::NewArray { len } => write!(f, "new_array len={}", len),
        MirOp::CreateClosure { func_idx } => write!(f, "create_closure func={}", func_idx),
        MirOp::CreateArguments => write!(f, "create_arguments"),
        MirOp::DefineProperty { obj, key, val } => {
            write!(f, "define_property {} {} {}", obj, key, val)
        }
        MirOp::SetPrototype { obj, proto } => write!(f, "set_prototype {} {}", obj, proto),

        // Type operations
        MirOp::TypeOf(v) => write!(f, "typeof {}", v),
        MirOp::InstanceOf { lhs, rhs, ic_index } => {
            write!(f, "instanceof {} {} ic={}", lhs, rhs, ic_index)
        }
        MirOp::In { key, obj, ic_index } => write!(f, "in {} {} ic={}", key, obj, ic_index),
        MirOp::ToNumber(v) => write!(f, "to_number {}", v),
        MirOp::ToStringOp(v) => write!(f, "to_string {}", v),
        MirOp::RequireCoercible(v) => write!(f, "require_coercible {}", v),

        // Control flow
        MirOp::Jump(target) => write!(f, "jump {}", target),
        MirOp::Branch {
            cond,
            true_block,
            false_block,
        } => {
            write!(
                f,
                "branch {} then={} else={}",
                cond, true_block, false_block
            )
        }
        MirOp::Return(v) => write!(f, "return {}", v),
        MirOp::ReturnUndefined => write!(f, "return_undefined"),
        MirOp::Deopt(d) => write!(f, "deopt {}", d),

        // Exception handling
        MirOp::TryStart { catch_block } => write!(f, "try_start catch={}", catch_block),
        MirOp::TryEnd => write!(f, "try_end"),
        MirOp::Throw(v) => write!(f, "throw {}", v),
        MirOp::Catch => write!(f, "catch"),

        // Iteration
        MirOp::GetIterator(v) => write!(f, "get_iterator {}", v),
        MirOp::IteratorNext(v) => write!(f, "iterator_next {}", v),
        MirOp::IteratorClose(v) => write!(f, "iterator_close {}", v),

        // GC
        MirOp::Safepoint { live } => {
            write!(f, "safepoint [{}]", format_args_list(live))
        }
        MirOp::WriteBarrier(v) => write!(f, "write_barrier {}", v),

        // Phi
        MirOp::Phi(inputs) => {
            write!(f, "phi")?;
            for (block, val) in inputs {
                write!(f, " [{}:{} ]", block, val)?;
            }
            Ok(())
        }

        // Misc
        MirOp::Move(v) => write!(f, "move {}", v),
        MirOp::LoadConstPool(idx) => write!(f, "load_const_pool {}", idx),
        MirOp::Spread(v) => write!(f, "spread {}", v),
        MirOp::HelperCall { kind, args } => {
            write!(f, "helper {:?} ({})", kind, format_args_list(args))
        }
    }
}

fn format_args_list(args: &[ValueId]) -> String {
    args.iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
