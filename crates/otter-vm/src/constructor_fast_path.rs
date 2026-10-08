//! Fast construction metadata for base and derived field initializers.
//!
//! This module recognizes bytecode constructors whose whole observable body is
//! a sequence of own data writes to `this` followed by `return undefined`, and
//! separately recognizes exact `this` stores for receiver capacity and shape
//! preparation. Executable property feedback owns the generated store path.
//!
//! # Contents
//! - [`SimpleConstructorInit`] — ordered property initializers.
//! - [`match_simple_constructor_init`] — conservative bytecode matcher.
//! - [`match_constructor_shape_stores`] — effect-tolerant exact-store matcher.
//!
//! # Invariants
//! - Simple pre-shaped initialization admits only base, ordinary, non-eval
//!   constructors; capacity matching may also recognize derived own stores.
//! - Every property write must target the `this` value loaded in the same body.
//! - The fast path preserves the normal prototype lookup before allocation.
//! - Generated linkage may install the final shape with undefined slots before
//!   entry only after proving every initializer name absent from the selected
//!   prototype chain; the body overwrites those slots before any observation.
//! - Derived and non-simple store names reserve physical receiver capacity.
//!   Property CacheIR and shared transition probes guard the actual store at
//!   its original operation; this matcher publishes no generated store plan.
//!
//! # See also
//! - [`crate::call_ops`]
//! - [`crate::object::ShapeRuntime`]

use otter_bytecode::{
    Op,
    opcode_schema::{RegisterSource, opcode_schema},
};

use crate::ExecutionContext;
use crate::executable::CodeBlock;

#[derive(Clone, Debug)]
pub(crate) struct SimpleConstructorInit {
    pub(crate) fields: Vec<SimpleConstructorField>,
}

#[derive(Clone, Debug)]
pub(crate) struct SimpleConstructorField {
    pub(crate) name: String,
}

/// One named store whose receiver is proven to be the current constructor's
/// `this` value at that exact bytecode operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConstructorShapeStore {
    pub(crate) name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegisterValue {
    Unknown,
    This,
    Param,
    Int32,
    Undefined,
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
        || !function.scopes.is_empty()
        || function.code.len() < 2
    {
        return None;
    }

    let mut registers = vec![RegisterValue::Unknown; function.register_count as usize];
    for slot in registers.iter_mut().take(function.param_count as usize) {
        *slot = RegisterValue::Param;
    }

    let mut fields: Vec<SimpleConstructorField> = Vec::new();
    for instr in &function.code {
        match function.op(instr) {
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
                context.exec_imm32(instr, 1)?;
                *registers.get_mut(dst)? = RegisterValue::Int32;
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
                // Only parameters and immediates admit installing the final
                // shape before the body: neither can observe the receiver.
                match *registers.get(src)? {
                    RegisterValue::Param | RegisterValue::Int32 | RegisterValue::Undefined => {}
                    RegisterValue::Unknown | RegisterValue::This => return None,
                }
                fields.push(SimpleConstructorField { name });
            }
            Op::ReturnUndefined => {
                return (!fields.is_empty()).then_some(SimpleConstructorInit { fields });
            }
            _ => return None,
        }
    }

    None
}

/// Recognize own constructor field names without moving observable stores.
///
/// Unlike [`match_simple_constructor_init`], this matcher does not require the
/// complete body to be side-effect free: it only supplies receiver capacity
/// and shape metadata. Ordinary property semantics still commit each store
/// at its source operation. Unknown operations only invalidate registers they
/// declare as outputs, so an intervening value computation cannot erase a
/// separately loaded `this` identity.
pub(crate) fn match_constructor_shape_stores(
    context: &ExecutionContext,
    function: &CodeBlock,
) -> Vec<ConstructorShapeStore> {
    if function.contains_direct_eval {
        return Vec::new();
    }
    let mut registers = vec![RegisterValue::Unknown; function.register_count as usize];
    let mut stores = Vec::new();
    for instr in &function.code {
        let op = function.op(instr);
        match op {
            Op::LoadThis => {
                if let Some(dst) = context.exec_register(instr, 0)
                    && let Some(slot) = registers.get_mut(dst as usize)
                {
                    *slot = RegisterValue::This;
                }
                continue;
            }
            Op::StoreLocal => {
                let Some(src) = context.exec_register(instr, 0) else {
                    continue;
                };
                let Some(local) = context.exec_imm32(instr, 1) else {
                    continue;
                };
                if local >= 0 {
                    let value = registers
                        .get(src as usize)
                        .copied()
                        .unwrap_or(RegisterValue::Unknown);
                    if let Some(slot) = registers.get_mut(local as usize) {
                        *slot = value;
                    }
                }
                continue;
            }
            Op::LoadLocal => {
                let (Some(dst), Some(local)) = (
                    context.exec_register(instr, 0),
                    context.exec_imm32(instr, 1),
                ) else {
                    continue;
                };
                let value = usize::try_from(local)
                    .ok()
                    .and_then(|local| registers.get(local).copied())
                    .unwrap_or(RegisterValue::Unknown);
                if let Some(slot) = registers.get_mut(dst as usize) {
                    *slot = value;
                }
                continue;
            }
            Op::StoreProperty => {
                let receiver = context.exec_register(instr, 0);
                if receiver.is_some_and(|receiver| {
                    registers.get(receiver as usize) == Some(&RegisterValue::This)
                }) && let Some(name_index) = context.exec_const_index(instr, 1)
                    && let Some(name) = context.string_constant_str(name_index)
                    && name != "__proto__"
                    && !stores
                        .iter()
                        .any(|store: &ConstructorShapeStore| store.name == name)
                {
                    stores.push(ConstructorShapeStore {
                        name: name.to_owned(),
                    });
                }
            }
            _ => {}
        }

        if let Some(specs) = opcode_schema(op).operand_shape.prefix() {
            for (operand, spec) in specs.iter().enumerate() {
                if !spec.register_access.writes() {
                    continue;
                }
                let register = match spec.register_source {
                    Some(RegisterSource::RegisterOperand) => {
                        context.exec_register(instr, operand).map(usize::from)
                    }
                    Some(RegisterSource::Imm32RegisterIndex) => context
                        .exec_imm32(instr, operand)
                        .and_then(|value| usize::try_from(value).ok()),
                    None => None,
                };
                if let Some(register) = register {
                    let width = usize::from(spec.register_access.width());
                    for slot in registers.iter_mut().skip(register).take(width) {
                        *slot = RegisterValue::Unknown;
                    }
                }
            }
        }
    }
    stores
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{
        ArgumentsObjectKind, BytecodeModule, Constant, Function, Instruction, Op, Operand,
        SourceKind,
    };

    use super::{match_constructor_shape_stores, match_simple_constructor_init};
    use crate::ExecutionContext;

    fn instr(pc: u32, op: Op, operands: impl AsRef<[Operand]>) -> Instruction {
        Instruction {
            pc,
            op,
            operands: operands.as_ref().to_vec(),
        }
    }

    fn string_constant(text: &str) -> Constant {
        Constant::String {
            utf16: text.encode_utf16().collect(),
        }
    }

    fn context_for(code: Vec<Instruction>) -> ExecutionContext {
        ExecutionContext::from_module(
            BytecodeModule {
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
                    scopes: Vec::new(),
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
                    source_text_range: None,
                    source_text_span: None,
                    module_url: String::new(),
                    contains_direct_eval: false,
                    primordial_iteration: false,
                    ignores_this: false,
                    asm_module: false,
                    code: code.into(),
                    spans: otter_bytecode::SpanTable::default(),
                    handlers: Vec::new(),
                    number_hint_sites: Vec::new(),
                    class_hint_sites: Vec::new(),
                }],
                constants: vec![
                    string_constant("x"),
                    string_constant("y"),
                    string_constant("tag"),
                ],
                module_resolutions: Vec::new(),
                module_inits: Vec::new(),
                function_source: None,
            },
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture")
    }

    #[test]
    fn tracks_this_across_unrelated_field_value_computation() {
        let context = context_for(vec![
            instr(0, Op::LoadThis, [Operand::Register(4)]),
            instr(
                1,
                Op::AddImm,
                [
                    Operand::Register(5),
                    Operand::Register(0),
                    Operand::Imm32(1),
                ],
            ),
            instr(
                2,
                Op::StoreProperty,
                [
                    Operand::Register(4),
                    Operand::ConstIndex(0),
                    Operand::Register(5),
                    Operand::Register(6),
                ],
            ),
            instr(3, Op::ReturnUndefined, []),
        ]);
        let function = context.exec_function(0).expect("test constructor");

        let stores = match_constructor_shape_stores(&context, function);

        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].name, "x");
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
        assert_eq!(init.fields[1].name, "y");
        assert_eq!(init.fields[2].name, "tag");
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
