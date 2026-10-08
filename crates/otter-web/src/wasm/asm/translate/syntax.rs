//! Literal classification and the lookup tables translation shares:
//! stdlib names and overloads, heap view loads and stores.

use super::*;

/// The stdlib constructor of a heap view.
pub(super) fn view_constructor(view: HeapView) -> StdlibFunction {
    match view {
        HeapView::Int8 => StdlibFunction::Int8Array,
        HeapView::Uint8 => StdlibFunction::Uint8Array,
        HeapView::Int16 => StdlibFunction::Int16Array,
        HeapView::Uint16 => StdlibFunction::Uint16Array,
        HeapView::Int32 => StdlibFunction::Int32Array,
        HeapView::Uint32 => StdlibFunction::Uint32Array,
        HeapView::Float32 => StdlibFunction::Float32Array,
        HeapView::Float64 => StdlibFunction::Float64Array,
    }
}

pub(super) fn math_constant(name: &str) -> Option<(&'static str, f64)> {
    use std::f64::consts;
    Some(match name {
        "E" => ("E", consts::E),
        "LN10" => ("LN10", consts::LN_10),
        "LN2" => ("LN2", consts::LN_2),
        "LOG2E" => ("LOG2E", consts::LOG2_E),
        "LOG10E" => ("LOG10E", consts::LOG10_E),
        "PI" => ("PI", consts::PI),
        "SQRT1_2" => ("SQRT1_2", consts::FRAC_1_SQRT_2),
        "SQRT2" => ("SQRT2", consts::SQRT_2),
        _ => return None,
    })
}

pub(super) fn math_function(name: &str) -> Option<StdlibFunction> {
    Some(match name {
        "acos" => StdlibFunction::Acos,
        "asin" => StdlibFunction::Asin,
        "atan" => StdlibFunction::Atan,
        "cos" => StdlibFunction::Cos,
        "sin" => StdlibFunction::Sin,
        "tan" => StdlibFunction::Tan,
        "exp" => StdlibFunction::Exp,
        "log" => StdlibFunction::Log,
        "ceil" => StdlibFunction::Ceil,
        "floor" => StdlibFunction::Floor,
        "sqrt" => StdlibFunction::Sqrt,
        "abs" => StdlibFunction::Abs,
        "min" => StdlibFunction::Min,
        "max" => StdlibFunction::Max,
        "atan2" => StdlibFunction::Atan2,
        "pow" => StdlibFunction::Pow,
        "imul" => StdlibFunction::Imul,
        "clz32" => StdlibFunction::Clz32,
        "fround" => StdlibFunction::Fround,
        _ => return None,
    })
}

pub(super) fn is_value_type(ty: AsmType) -> bool {
    ty.is_a(AsmType::INT) || ty.is_a(AsmType::FLOAT) || ty.is_a(AsmType::DOUBLE)
}

/// `true` for a module `var` whose initializer is an array literal: the
/// function tables after the functions.
pub(super) fn is_function_table(declaration: &VariableDeclaration<'_>) -> bool {
    declaration
        .declarations
        .iter()
        .any(|declarator| matches!(declarator.init, Some(Expression::ArrayExpression(_))))
}

pub(super) fn table_declarator<'a>(
    declaration: &'a VariableDeclaration<'a>,
    declarator: &'a oxc_ast::ast::VariableDeclarator<'a>,
) -> Result<(&'a str, Vec<&'a str>)> {
    if declaration.kind != oxc_ast::ast::VariableDeclarationKind::Var {
        return fail("function table must be a var");
    }
    let BindingPattern::BindingIdentifier(id) = &declarator.id else {
        return fail("bad function table");
    };
    let Some(Expression::ArrayExpression(array)) = &declarator.init else {
        return fail("expected function table");
    };
    let mut elements = Vec::new();
    for element in &array.elements {
        let ArrayExpressionElement::Identifier(function) = element else {
            return fail("function table element is not a name");
        };
        elements.push(function.name.as_str());
    }
    Ok((id.name.as_str(), elements))
}

/// A numeric literal as the asm.js scanner reads it.
pub(super) enum Literal {
    Double(f64),
    Unsigned(u32),
}

/// A literal is a double when its source has a `.` or its value is not an
/// integer; otherwise it is an unsigned integer up to 2^32 - 1.
pub(super) fn classify(literal: &NumericLiteral<'_>) -> Result<Literal> {
    let raw = literal.raw.as_ref().map_or("", |raw| raw.as_str());
    let value = literal.value;
    if raw.contains('.') || value.trunc() != value {
        return Ok(Literal::Double(value));
    }
    if value > f64::from(u32::MAX) {
        return fail("integer literal out of range");
    }
    Ok(Literal::Unsigned(value as u32))
}

pub(super) fn unsigned_literal(expression: &Expression<'_>) -> Option<u32> {
    match expression {
        Expression::NumericLiteral(literal) => match classify(literal) {
            Ok(Literal::Unsigned(value)) => Some(value),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn is_zero(expression: &Expression<'_>) -> bool {
    unsigned_literal(expression) == Some(0)
}

/// The value of `fround(<literal>)` / `fround(-<literal>)`.
pub(super) fn fround_literal(call: &CallExpression<'_>) -> Result<f32> {
    let [argument] = call.arguments.as_slice() else {
        return fail("fround takes one argument");
    };
    let (negate, literal) = match argument {
        Argument::NumericLiteral(literal) => (false, literal),
        Argument::UnaryExpression(unary) if unary.operator == UnaryOperator::UnaryNegation => {
            let Expression::NumericLiteral(literal) = &unary.argument else {
                return fail("expected numeric literal");
            };
            (true, literal)
        }
        _ => return fail("expected numeric literal"),
    };
    let value = match classify(literal)? {
        Literal::Double(value) => value,
        Literal::Unsigned(value) if value <= 0x7FFF_FFFF => f64::from(value),
        Literal::Unsigned(_) => return fail("numeric literal out of range"),
    };
    Ok((if negate { -value } else { value }) as f32)
}

pub(super) fn init_expression<'b, 'a>(
    init: &'b ForStatementInit<'a>,
) -> Option<&'b Expression<'a>> {
    init.as_expression()
}

/// `case <n>:` / `case -<n>:` as a signed value.
pub(super) fn case_value(test: &Expression<'_>) -> Result<i32> {
    if let Some(value) = unsigned_literal(test) {
        if value > 0x7FFF_FFFF {
            return fail("case value out of range");
        }
        return Ok(value as i32);
    }
    if let Expression::UnaryExpression(unary) = test
        && unary.operator == UnaryOperator::UnaryNegation
        && let Some(value) = unsigned_literal(&unary.argument)
    {
        if value > 0x8000_0000 {
            return fail("case value out of range");
        }
        return Ok((value as i32).wrapping_neg());
    }
    fail("expected numeric case")
}

/// An integer `+`/`-` node or integer negation, possibly parenthesized: its
/// `intish` value is an exact sum of `int`s.
pub(super) fn additive_chain(expression: &Expression<'_>) -> bool {
    let mut expression = expression;
    while let Expression::ParenthesizedExpression(inner) = expression {
        expression = &inner.expression;
    }
    match expression {
        Expression::BinaryExpression(binary) => {
            matches!(
                binary.operator,
                BinaryOperator::Addition | BinaryOperator::Subtraction
            )
        }
        Expression::UnaryExpression(unary) => unary.operator == UnaryOperator::UnaryNegation,
        _ => false,
    }
}

/// An additive operand that is an `int`, or the `intish` result of a nested
/// integer addition or negation. A load's `intish` is not: out of bounds it
/// is `undefined`, which addition does not treat as `0`.
pub(super) fn additive_int(ty: AsmType, expression: &Expression<'_>) -> bool {
    ty.is_a(AsmType::INT) || (ty.is_a(AsmType::INTISH) && additive_chain(expression))
}

pub(super) fn comparison(
    operator: BinaryOperator,
    a: AsmType,
    b: AsmType,
) -> Result<Instruction<'static>> {
    use BinaryOperator as B;
    use Instruction as I;
    let both = |ty: AsmType| a.is_a(ty) && b.is_a(ty);
    let row = if both(AsmType::SIGNED) {
        0
    } else if both(AsmType::UNSIGNED) {
        1
    } else if both(AsmType::DOUBLE) {
        2
    } else if both(AsmType::FLOAT) {
        3
    } else {
        return fail("expected signed, unsigned, double or float comparison");
    };
    let table: [Instruction<'static>; 4] = match operator {
        B::LessThan => [I::I32LtS, I::I32LtU, I::F64Lt, I::F32Lt],
        B::LessEqualThan => [I::I32LeS, I::I32LeU, I::F64Le, I::F32Le],
        B::GreaterThan => [I::I32GtS, I::I32GtU, I::F64Gt, I::F32Gt],
        B::GreaterEqualThan => [I::I32GeS, I::I32GeU, I::F64Ge, I::F32Ge],
        B::Equality => [I::I32Eq, I::I32Eq, I::F64Eq, I::F32Eq],
        _ => [I::I32Ne, I::I32Ne, I::F64Ne, I::F32Ne],
    };
    Ok(table[row].clone())
}

/// V8's stdlib overload sets (with the errata for `min`/`max`/`abs`/`ceil`).
pub(super) fn stdlib_overload(function: StdlibFunction, result: AsmType, args: &[AsmType]) -> bool {
    use StdlibFunction as F;
    let unary =
        |param: AsmType, ret: AsmType| result == ret && args.len() == 1 && args[0].is_a(param);
    match function {
        F::Acos | F::Asin | F::Atan | F::Cos | F::Sin | F::Tan | F::Exp | F::Log => {
            unary(AsmType::DOUBLEQ, AsmType::DOUBLE)
        }
        F::Atan2 | F::Pow => {
            result == AsmType::DOUBLE
                && args.len() == 2
                && args.iter().all(|arg| arg.is_a(AsmType::DOUBLEQ))
        }
        F::Imul => {
            result == AsmType::SIGNED
                && args.len() == 2
                && args.iter().all(|arg| arg.is_a(AsmType::INT))
        }
        F::Clz32 => unary(AsmType::INT, AsmType::SIGNED),
        F::Ceil | F::Floor | F::Sqrt => {
            unary(AsmType::DOUBLEQ, AsmType::DOUBLE) || unary(AsmType::FLOATQ, AsmType::FLOATISH)
        }
        F::Abs => {
            unary(AsmType::SIGNED, AsmType::UNSIGNED)
                || unary(AsmType::DOUBLEQ, AsmType::DOUBLE)
                || unary(AsmType::FLOATQ, AsmType::FLOATISH)
        }
        F::Min | F::Max => {
            args.len() >= 2
                && [AsmType::SIGNED, AsmType::FLOAT, AsmType::DOUBLE]
                    .into_iter()
                    .any(|ty| result == ty && args.iter().all(|arg| arg.is_a(ty)))
        }
        _ => false,
    }
}

pub(super) fn math_name(function: StdlibFunction) -> &'static str {
    use StdlibFunction as F;
    match function {
        F::Acos => "acos",
        F::Asin => "asin",
        F::Atan => "atan",
        F::Cos => "cos",
        F::Sin => "sin",
        F::Tan => "tan",
        F::Exp => "exp",
        F::Log => "log",
        F::Atan2 => "atan2",
        _ => "pow",
    }
}

pub(super) fn view_val_type(view: HeapView) -> ValType {
    match view {
        HeapView::Float32 => ValType::F32,
        HeapView::Float64 => ValType::F64,
        _ => ValType::I32,
    }
}

pub(super) fn mem_arg(view: HeapView) -> MemArg {
    MemArg {
        offset: 0,
        align: view.size().trailing_zeros(),
        memory_index: 0,
    }
}

pub(super) fn load_instruction(view: HeapView) -> Instruction<'static> {
    let arg = mem_arg(view);
    match view {
        HeapView::Int8 => Instruction::I32Load8S(arg),
        HeapView::Uint8 => Instruction::I32Load8U(arg),
        HeapView::Int16 => Instruction::I32Load16S(arg),
        HeapView::Uint16 => Instruction::I32Load16U(arg),
        HeapView::Int32 | HeapView::Uint32 => Instruction::I32Load(arg),
        HeapView::Float32 => Instruction::F32Load(arg),
        HeapView::Float64 => Instruction::F64Load(arg),
    }
}

pub(super) fn store_instruction(view: HeapView) -> Instruction<'static> {
    let arg = mem_arg(view);
    match view {
        HeapView::Int8 | HeapView::Uint8 => Instruction::I32Store8(arg),
        HeapView::Int16 | HeapView::Uint16 => Instruction::I32Store16(arg),
        HeapView::Int32 | HeapView::Uint32 => Instruction::I32Store(arg),
        HeapView::Float32 => Instruction::F32Store(arg),
        HeapView::Float64 => Instruction::F64Store(arg),
    }
}

/// A typed-array read past the end is `undefined`: `0` once coerced to an
/// integer, `NaN` as a number.
pub(super) fn out_of_bounds_value(view: HeapView) -> Instruction<'static> {
    match view {
        HeapView::Float32 => Instruction::F32Const(f32::NAN.into()),
        HeapView::Float64 => Instruction::F64Const(f64::NAN.into()),
        _ => Instruction::I32Const(0),
    }
}
