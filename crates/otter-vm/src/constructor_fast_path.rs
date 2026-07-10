//! Fast construction metadata for simple base-class initializers.
//!
//! This module recognizes bytecode constructors whose whole observable body is
//! a sequence of own data writes to `this` followed by `return undefined`.
//!
//! # Contents
//! - [`SimpleConstructorInit`] — ordered property initializers.
//! - [`match_simple_constructor_init`] — conservative bytecode matcher.
//!
//! # Invariants
//! - Only base, ordinary, non-eval constructors are eligible.
//! - Every property write must target the `this` value loaded in the same body.
//! - The fast path preserves the normal prototype lookup before allocation.
//!
//! # See also
//! - [`crate::call_ops`]
//! - [`crate::object::ShapeRuntime`]

use otter_bytecode::Op;

use crate::executable::CodeBlock;
use crate::{ExecutionContext, NumberValue, Value};

#[derive(Clone, Debug)]
pub(crate) struct SimpleConstructorInit {
    pub(crate) fields: Vec<SimpleConstructorField>,
}

#[derive(Clone, Debug)]
pub(crate) struct SimpleConstructorField {
    pub(crate) name: String,
    pub(crate) source: SimpleConstructorSource,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SimpleConstructorSource {
    Param(usize),
    Int32(i32),
    Undefined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegisterValue {
    Unknown,
    This,
    Param(usize),
    Int32(i32),
    Undefined,
}

impl SimpleConstructorSource {
    pub(crate) fn resolve(self, args: &[Value]) -> Value {
        match self {
            Self::Param(index) => args.get(index).copied().unwrap_or_else(Value::undefined),
            Self::Int32(value) => Value::number(NumberValue::Smi(value)),
            Self::Undefined => Value::undefined(),
        }
    }
}

pub(crate) fn match_simple_constructor_init(
    context: &ExecutionContext,
    function: &CodeBlock,
) -> Option<SimpleConstructorInit> {
    if function.is_derived_constructor
        || function.is_method
        || function.has_rest
        || function.needs_arguments
        || function.is_async
        || function.is_generator
        || function.is_async_generator
        || function.contains_direct_eval
        || function.own_upvalue_count != 0
        || function.code.len() < 2
    {
        return None;
    }

    let mut registers = vec![RegisterValue::Unknown; function.register_count as usize];
    for (index, slot) in registers
        .iter_mut()
        .take(function.param_count as usize)
        .enumerate()
    {
        *slot = RegisterValue::Param(index);
    }

    let mut fields: Vec<SimpleConstructorField> = Vec::new();
    for instr in &function.code {
        match instr.op() {
            Op::LoadThis => {
                let dst = context.exec_register(instr, 0)? as usize;
                *registers.get_mut(dst)? = RegisterValue::This;
            }
            Op::StoreLocal => {
                let src = context.exec_register(instr, 0)? as usize;
                let local = context.exec_imm32(instr, 1)?;
                if local < 0 {
                    return None;
                }
                let value = *registers.get(src)?;
                *registers.get_mut(local as usize)? = value;
            }
            Op::LoadLocal => {
                let dst = context.exec_register(instr, 0)? as usize;
                let local = context.exec_imm32(instr, 1)?;
                if local < 0 {
                    return None;
                }
                let value = *registers.get(local as usize)?;
                *registers.get_mut(dst)? = value;
            }
            Op::LoadInt32 => {
                let dst = context.exec_register(instr, 0)? as usize;
                let value = context.exec_imm32(instr, 1)?;
                *registers.get_mut(dst)? = RegisterValue::Int32(value);
            }
            Op::LoadUndefined => {
                let dst = context.exec_register(instr, 0)? as usize;
                *registers.get_mut(dst)? = RegisterValue::Undefined;
            }
            Op::StoreProperty => {
                let obj = context.exec_register(instr, 0)? as usize;
                if *registers.get(obj)? != RegisterValue::This {
                    return None;
                }
                let name_idx = context.exec_const_index(instr, 1)?;
                let name = context.string_constant_str(name_idx)?.to_owned();
                if name == "__proto__" || fields.iter().any(|field| field.name == name) {
                    return None;
                }
                let src = context.exec_register(instr, 2)? as usize;
                let source = match *registers.get(src)? {
                    RegisterValue::Param(index) => SimpleConstructorSource::Param(index),
                    RegisterValue::Int32(value) => SimpleConstructorSource::Int32(value),
                    RegisterValue::Undefined => SimpleConstructorSource::Undefined,
                    RegisterValue::Unknown | RegisterValue::This => return None,
                };
                fields.push(SimpleConstructorField { name, source });
            }
            Op::ReturnUndefined => {
                return (!fields.is_empty()).then_some(SimpleConstructorInit { fields });
            }
            _ => return None,
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{
        ArgumentsObjectKind, BytecodeModule, Constant, Function, Instruction, Op, Operand,
        OperandList, SourceKind,
    };

    use super::{SimpleConstructorSource, match_simple_constructor_init};
    use crate::ExecutionContext;

    fn instr(pc: u32, op: Op, operands: impl Into<OperandList>) -> Instruction {
        Instruction {
            pc,
            op,
            operands: operands.into(),
        }
    }

    fn string_constant(text: &str) -> Constant {
        Constant::String {
            utf16: text.encode_utf16().collect(),
        }
    }

    fn context_for(code: Vec<Instruction>) -> ExecutionContext {
        ExecutionContext::from_module(BytecodeModule {
            module: "<ctor-fast-path-test>".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "Point".to_string(),
                span: (0, 0),
                locals: 2,
                scratch: 11,
                param_count: 2,
                length: 2,
                own_upvalue_count: 0,
                is_strict: true,
                is_arrow: false,
                is_method: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_derived_constructor: false,
                is_module: false,
                needs_arguments: false,
                uses_arguments_callee: false,
                arguments_object_kind: ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                source_text: None,
                source_text_span: None,
                module_url: String::new(),
                direct_eval_bindings: Vec::new(),
                contains_direct_eval: false,
                code,
                spans: Vec::new(),
            }],
            constants: vec![
                string_constant("x"),
                string_constant("y"),
                string_constant("tag"),
            ],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    #[test]
    fn matches_point_style_field_initializer() {
        let context = context_for(vec![
            instr(0, Op::StoreLocal, [Operand::Register(0), Operand::Imm32(2)]),
            instr(1, Op::StoreLocal, [Operand::Register(1), Operand::Imm32(3)]),
            instr(2, Op::LoadThis, [Operand::Register(4)]),
            instr(3, Op::LoadLocal, [Operand::Register(5), Operand::Imm32(2)]),
            instr(
                4,
                Op::StoreProperty,
                [
                    Operand::Register(4),
                    Operand::ConstIndex(0),
                    Operand::Register(5),
                    Operand::Register(6),
                ],
            ),
            instr(5, Op::LoadThis, [Operand::Register(7)]),
            instr(6, Op::LoadLocal, [Operand::Register(8), Operand::Imm32(3)]),
            instr(
                7,
                Op::StoreProperty,
                [
                    Operand::Register(7),
                    Operand::ConstIndex(1),
                    Operand::Register(8),
                    Operand::Register(9),
                ],
            ),
            instr(8, Op::LoadThis, [Operand::Register(10)]),
            instr(9, Op::LoadInt32, [Operand::Register(11), Operand::Imm32(0)]),
            instr(
                10,
                Op::StoreProperty,
                [
                    Operand::Register(10),
                    Operand::ConstIndex(2),
                    Operand::Register(11),
                    Operand::Register(12),
                ],
            ),
            instr(11, Op::ReturnUndefined, []),
        ]);
        let function = context.exec_function(0).expect("function exists");

        let init = match_simple_constructor_init(&context, function).expect("matches");
        assert_eq!(init.fields.len(), 3);
        assert_eq!(init.fields[0].name, "x");
        assert!(matches!(
            init.fields[0].source,
            SimpleConstructorSource::Param(0)
        ));
        assert_eq!(init.fields[1].name, "y");
        assert!(matches!(
            init.fields[1].source,
            SimpleConstructorSource::Param(1)
        ));
        assert_eq!(init.fields[2].name, "tag");
        assert!(matches!(
            init.fields[2].source,
            SimpleConstructorSource::Int32(0)
        ));
    }

    #[test]
    fn rejects_duplicate_field_initializers() {
        let context = context_for(vec![
            instr(0, Op::LoadThis, [Operand::Register(2)]),
            instr(1, Op::LoadLocal, [Operand::Register(3), Operand::Imm32(0)]),
            instr(
                2,
                Op::StoreProperty,
                [
                    Operand::Register(2),
                    Operand::ConstIndex(0),
                    Operand::Register(3),
                    Operand::Register(4),
                ],
            ),
            instr(3, Op::LoadThis, [Operand::Register(5)]),
            instr(4, Op::LoadInt32, [Operand::Register(6), Operand::Imm32(1)]),
            instr(
                5,
                Op::StoreProperty,
                [
                    Operand::Register(5),
                    Operand::ConstIndex(0),
                    Operand::Register(6),
                    Operand::Register(7),
                ],
            ),
            instr(6, Op::ReturnUndefined, []),
        ]);
        let function = context.exec_function(0).expect("function exists");

        assert!(match_simple_constructor_init(&context, function).is_none());
    }
}
