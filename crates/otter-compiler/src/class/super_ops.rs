//! Super-call and super-member lowering helpers for class methods.
//!
//! # Contents
//! - [`compile_super_call`] - lower `super(args...)` constructor calls.
//! - [`compile_super_method_call`] - lower `super.method(args...)` calls.
//! - [`compile_super_member_load`] - lower `super.name` property reads.
//! - [`load_super_method`] - load a method from the parent prototype.
//!
//! # Invariants
//! - Super operations resolve through the synthetic class captures installed by the parent class lowering.
//! - Calls preserve the current frame's `this` binding.
//!
//! # See also
//! - [`super`]

use super::{SUPER_CTOR_NAME, SUPER_HOME_NAME, load_synthetic_capture};
use crate::*;

/// `super(args...)` lowering. Per §13.3.7.3 SuperCall, super-calls
/// must run `[[Construct]]` on the parent constructor with the
/// derived class's `new.target` inherited from the enclosing frame
/// — NOT an ordinary `[[Call]]`. Lower to `Op::SuperConstructSpread`
/// after packing every argument (spread or not) into a fresh array
/// so the VM dispatch picks up `frame.new_target` and routes through
/// `dispatch_construct_with_new_target`. The result of super() is
/// the constructed value, which the dispatcher returns directly.
pub(crate) fn compile_super_call(
    cx: &mut Compiler,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let super_ctor = load_synthetic_capture(cx, SUPER_CTOR_NAME, span)?;
    let args_reg = compile_spread_call_args(cx, arguments, span)?;
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::SuperConstructSpread,
        vec![
            Operand::Register(dst),
            Operand::Register(super_ctor),
            Operand::Register(args_reg),
        ],
        span,
    );
    Ok(dst)
}

/// `this = current frame's this`.
pub(crate) fn compile_super_method_call(
    cx: &mut Compiler,
    method_name: &str,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let method_reg = load_super_method(cx, method_name, span)?;
    let this_reg = cx.alloc_scratch();
    cx.emit(Op::LoadThis, [Operand::Register(this_reg)], span);
    let has_spread = arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
    let dst = cx.alloc_scratch();
    if has_spread {
        let args_reg = compile_spread_call_args(cx, arguments, span)?;
        cx.emit(
            Op::CallSpread,
            vec![
                Operand::Register(dst),
                Operand::Register(method_reg),
                Operand::Register(this_reg),
                Operand::Register(args_reg),
            ],
            span,
        );
    } else {
        let arg_regs = compile_call_args(cx, arguments, span)?;
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(method_reg));
        operands.push(Operand::Register(this_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
    }
    Ok(dst)
}

/// `super[expr](args...)` — invoke a parent-prototype method resolved
/// through a computed key, with `this` bound to the current receiver
/// (§13.3.7.1 / §13.3.5 MakeSuperPropertyReference). Mirrors
/// [`compile_super_method_call`] but loads the method via
/// `GetPrototype(home)` + `LoadElement`.
pub(crate) fn compile_super_computed_method_call(
    cx: &mut Compiler,
    key_expr: &oxc_ast::ast::Expression<'_>,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let home_reg = load_synthetic_capture(cx, SUPER_HOME_NAME, span)?;
    let parent_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetPrototype,
        [Operand::Register(parent_reg), Operand::Register(home_reg)],
        span,
    );
    let idx = compile_expr(cx, key_expr, span)?;
    let method_reg = cx.alloc_scratch();
    cx.emit(
        Op::LoadElement,
        vec![
            Operand::Register(method_reg),
            Operand::Register(parent_reg),
            Operand::Register(idx),
        ],
        span,
    );
    let this_reg = cx.alloc_scratch();
    cx.emit(Op::LoadThis, [Operand::Register(this_reg)], span);
    let has_spread = arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
    let dst = cx.alloc_scratch();
    if has_spread {
        let args_reg = compile_spread_call_args(cx, arguments, span)?;
        cx.emit(
            Op::CallSpread,
            vec![
                Operand::Register(dst),
                Operand::Register(method_reg),
                Operand::Register(this_reg),
                Operand::Register(args_reg),
            ],
            span,
        );
    } else {
        let arg_regs = compile_call_args(cx, arguments, span)?;
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(method_reg));
        operands.push(Operand::Register(this_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
    }
    Ok(dst)
}

/// load. Resolves to a register holding the looked-up value.
pub(crate) fn compile_super_member_load(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    load_super_method(cx, name, span)
}

/// the `super` shape stays bytecode-readable.
pub(crate) fn load_super_method(
    cx: &mut Compiler,
    name: &str,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let home_reg = load_synthetic_capture(cx, SUPER_HOME_NAME, span)?;
    let parent_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetPrototype,
        [Operand::Register(parent_reg), Operand::Register(home_reg)],
        span,
    );
    let name_idx = cx.intern_string_constant(name);
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::LoadProperty,
        vec![
            Operand::Register(dst),
            Operand::Register(parent_reg),
            Operand::ConstIndex(name_idx),
        ],
        span,
    );
    Ok(dst)
}
