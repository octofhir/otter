//! Module variables (asm.js §6.1): heap views, standard-library members,
//! foreign imports and constant globals.
//!
//! # Invariants
//! - A `foreign` member is read once, in declaration order, as the body
//!   would read it; values import as immutable wasm globals that initialize
//!   the module's mutable ones.

use super::*;

impl<'a> ModuleTranslator<'a> {
    pub(super) fn module_variables(
        &mut self,
        declaration: &'a VariableDeclaration<'a>,
    ) -> Result<()> {
        use oxc_ast::ast::VariableDeclarationKind as Kind;
        let mutable = match declaration.kind {
            Kind::Var => true,
            Kind::Const => false,
            _ => return fail("module variable must be var or const"),
        };
        for declarator in &declaration.declarations {
            let BindingPattern::BindingIdentifier(id) = &declarator.id else {
                return fail("module variable is not an identifier");
            };
            let Some(init) = &declarator.init else {
                return fail("module variable without initializer");
            };
            let binding = self.module_variable(init, mutable)?;
            self.declare(id.name.as_str(), binding)?;
        }
        Ok(())
    }

    pub(super) fn global(
        &mut self,
        ty: AsmType,
        val: ValType,
        init: ConstExpr,
        mutable: bool,
    ) -> Binding {
        let slot = self.define_global(val, init);
        Binding::Global { ty, slot, mutable }
    }

    pub(super) fn int_global(&mut self, value: i32, mutable: bool) -> Binding {
        let ty = if mutable {
            AsmType::INT
        } else {
            AsmType::SIGNED
        };
        self.global(ty, ValType::I32, ConstExpr::i32_const(value), mutable)
    }

    pub(super) fn module_variable(
        &mut self,
        init: &'a Expression<'a>,
        mutable: bool,
    ) -> Result<Binding> {
        match init {
            Expression::NumericLiteral(literal) => match classify(literal)? {
                Literal::Double(value) => Ok(self.global(
                    AsmType::DOUBLE,
                    ValType::F64,
                    ConstExpr::f64_const(value.into()),
                    mutable,
                )),
                Literal::Unsigned(value) if value <= 0x7FFF_FFFF => {
                    Ok(self.int_global(value as i32, mutable))
                }
                Literal::Unsigned(_) => fail("numeric literal out of range"),
            },
            Expression::UnaryExpression(unary)
                if unary.operator == UnaryOperator::UnaryNegation =>
            {
                let Expression::NumericLiteral(literal) = &unary.argument else {
                    return fail("expected numeric literal");
                };
                match classify(literal)? {
                    Literal::Double(value) => Ok(self.global(
                        AsmType::DOUBLE,
                        ValType::F64,
                        ConstExpr::f64_const((-value).into()),
                        mutable,
                    )),
                    // V8 reads `-0` as a float.
                    Literal::Unsigned(0) => Ok(self.global(
                        AsmType::FLOAT,
                        ValType::F32,
                        ConstExpr::f32_const((-0.0f32).into()),
                        mutable,
                    )),
                    Literal::Unsigned(value) if value <= 0x7FFF_FFFF => {
                        Ok(self.int_global(-(value as i32), mutable))
                    }
                    Literal::Unsigned(_) => fail("numeric literal out of range"),
                }
            }
            Expression::NewExpression(new) => {
                let Some(view) = self
                    .stdlib_member(&new.callee)
                    .and_then(HeapView::from_constructor)
                else {
                    return fail("expected a heap view constructor");
                };
                let [Argument::Identifier(heap)] = new.arguments.as_slice() else {
                    return fail("heap view takes the heap");
                };
                if Some(heap.name.as_str()) != self.heap_name {
                    return fail("heap view takes the heap");
                }
                self.stdlib
                    .push(StdlibUse::Function(view_constructor(view)));
                self.memory = true;
                Ok(Binding::View(view))
            }
            Expression::StaticMemberExpression(_) => {
                if let Some(name) = self.foreign_member(init) {
                    let index = self.add_foreign(name, ForeignKind::Function);
                    return Ok(Binding::Foreign(index));
                }
                self.stdlib_variable(init)
            }
            Expression::BinaryExpression(binary)
                if binary.operator == BinaryOperator::BitwiseOR && is_zero(&binary.right) =>
            {
                let Some(name) = self.foreign_member(&binary.left) else {
                    return fail("expected foreign import");
                };
                Ok(self.imported_global(name, false, mutable))
            }
            Expression::UnaryExpression(unary) if unary.operator == UnaryOperator::UnaryPlus => {
                let Some(name) = self.foreign_member(&unary.argument) else {
                    return fail("expected foreign import");
                };
                Ok(self.imported_global(name, true, mutable))
            }
            Expression::Identifier(id) => match self.names.get(id.name.as_str()).copied() {
                Some(Binding::Global {
                    ty,
                    slot,
                    mutable: false,
                }) if !mutable && is_value_type(ty) => Ok(Binding::Global {
                    ty,
                    slot,
                    mutable: false,
                }),
                Some(Binding::Constant(value)) if !mutable => Ok(Binding::Constant(value)),
                _ => fail("bad global initializer"),
            },
            Expression::CallExpression(call) if self.is_fround(&call.callee) => {
                let value = fround_literal(call)?;
                Ok(self.global(
                    AsmType::FLOAT,
                    ValType::F32,
                    ConstExpr::f32_const(value.into()),
                    mutable,
                ))
            }
            _ => fail("bad module variable"),
        }
    }

    pub(super) fn add_foreign(&mut self, name: &str, kind: ForeignKind) -> u32 {
        self.foreign.push(ForeignMember {
            name: name.to_string(),
            kind,
        });
        self.foreign.len() as u32 - 1
    }

    pub(super) fn imported_global(
        &mut self,
        name: &'a str,
        double: bool,
        mutable: bool,
    ) -> Binding {
        let (ty, val, kind) = if double {
            (AsmType::DOUBLE, ValType::F64, ForeignKind::Double)
        } else {
            (AsmType::INT, ValType::I32, ForeignKind::Int)
        };
        let foreign = self.add_foreign(name, kind);
        let import = self.global_imports.len() as u32;
        self.global_imports.push((foreign, val));
        self.global(ty, val, ConstExpr::global_get(import), mutable)
    }

    /// `foreign.<name>` → `name`.
    pub(super) fn foreign_member(&self, expression: &'a Expression<'a>) -> Option<&'a str> {
        let Expression::StaticMemberExpression(member) = expression else {
            return None;
        };
        let Expression::Identifier(object) = &member.object else {
            return None;
        };
        (Some(object.name.as_str()) == self.foreign_name && !member.optional)
            .then(|| member.property.name.as_str())
    }

    /// `stdlib.<name>` → `name`.
    pub(super) fn stdlib_member(&self, expression: &'a Expression<'a>) -> Option<&'a str> {
        let Expression::StaticMemberExpression(member) = expression else {
            return None;
        };
        let Expression::Identifier(object) = &member.object else {
            return None;
        };
        (Some(object.name.as_str()) == self.stdlib_name && !member.optional)
            .then(|| member.property.name.as_str())
    }

    /// `stdlib.Math.<name>` → `name`.
    pub(super) fn stdlib_math_member(&self, expression: &'a Expression<'a>) -> Option<&'a str> {
        let Expression::StaticMemberExpression(member) = expression else {
            return None;
        };
        (self.stdlib_member(&member.object) == Some("Math") && !member.optional)
            .then(|| member.property.name.as_str())
    }

    pub(super) fn stdlib_variable(&mut self, init: &'a Expression<'a>) -> Result<Binding> {
        if let Some(name) = self.stdlib_math_member(init) {
            if let Some((name, value)) = math_constant(name) {
                self.stdlib.push(StdlibUse::MathConstant(name, value));
                return Ok(Binding::Constant(value));
            }
            let Some(function) = math_function(name) else {
                return fail("invalid member of stdlib.Math");
            };
            self.stdlib.push(StdlibUse::Function(function));
            return Ok(Binding::Stdlib(function));
        }
        match self.stdlib_member(init) {
            Some("Infinity") => {
                self.stdlib.push(StdlibUse::Infinity);
                Ok(Binding::Constant(f64::INFINITY))
            }
            Some("NaN") => {
                self.stdlib.push(StdlibUse::NaN);
                Ok(Binding::Constant(f64::NAN))
            }
            _ => fail("invalid member of stdlib"),
        }
    }

    pub(super) fn is_fround(&self, callee: &Expression<'a>) -> bool {
        matches!(callee, Expression::Identifier(id)
            if matches!(self.names.get(id.name.as_str()), Some(Binding::Stdlib(StdlibFunction::Fround))))
    }
}
