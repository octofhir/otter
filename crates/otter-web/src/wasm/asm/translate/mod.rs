//! asm.js validation and translation to WebAssembly.
//!
//! A port of V8's `AsmJsParser` (`src/asmjs/asm-parser.cc`) that walks the
//! oxc AST of a `"use asm"` function instead of re-scanning tokens. Every
//! construct is type-checked against the asm.js lattice as it is emitted, so
//! a module either translates completely or is rejected and runs as ordinary
//! JavaScript. Translation happens at link time, against the actual heap
//! length, so heap bounds are immediates.
//!
//! # Contents
//! - [`translate`] — parse, validate and emit one module; module-level
//!   validation (asm.js §6.1–6.4) lives here.
//! - [`Translation`] / [`Import`] / [`StdlibUse`] / [`Exports`] — what the
//!   linker supplies and checks around the emitted module.
//! - `module_vars` — module variables: heap views, stdlib, foreign imports.
//! - `function` — function bodies and statements; `function::expression` —
//!   expressions, heap access and calls.
//! - `encode` — the WebAssembly binary.
//! - `syntax` — literal classification and asm.js/wasm lookup tables.
//!
//! # Invariants
//! - Every accepted program computes exactly what its JavaScript evaluation
//!   computes. Where V8 is laxer than that (an out-of-bounds `intish` load in
//!   an addition, `HEAPF32[i] = d` yielding the rounded value), validation is
//!   strict or emission follows JavaScript.
//! - Heap loads out of bounds yield `0` (integer views) or `NaN` (float
//!   views) and stores out of bounds are dropped, as typed-array accesses do;
//!   the module never traps on memory.
//! - Integer division and remainder never trap: `x / 0` and `x % 0` are `0`,
//!   `INT_MIN / -1` is `INT_MIN`, as `(x / y) | 0` is in JavaScript.
//! - Function indices of imports are known only after every body is emitted,
//!   so bodies are buffered as [`Op`]s with symbolic call targets and
//!   encoded last.
//!
//! # See also
//! - <http://asmjs.org/spec/latest/>
//! - `super::types` — the lattice; `super::instance` — the linker.

use std::borrow::Cow;
use std::collections::HashMap;

use otter_runtime::asm_stdlib::StdlibFunction;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, AssignmentTarget, BindingPattern, CallExpression,
    ComputedMemberExpression, Expression, ForStatementInit, Function, NumericLiteral,
    ObjectPropertyKind, PropertyKey, Statement, SwitchStatement, VariableDeclaration,
};
use oxc_parser::{ParseOptions, Parser};
use oxc_span::SourceType;
use oxc_syntax::operator::{AssignmentOperator, BinaryOperator, UnaryOperator};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, ElementSection, Elements, EntityType, ExportKind,
    ExportSection, Function as WasmFunction, FunctionSection, GlobalSection, GlobalType,
    ImportSection, Instruction, MemArg, MemorySection, MemoryType, Module as WasmModule, RefType,
    TableSection, TableType, TypeSection, ValType,
};

use super::types::{AsmType, HeapView, Signature};

mod encode;
mod function;
mod module_vars;
mod syntax;

use function::FunctionTranslator;
use syntax::*;

/// A rejected module: it runs as ordinary JavaScript. The text names the
/// first construct outside asm.js, for diagnostics and tests.
#[derive(Debug)]
pub(super) struct Invalid(
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests and debuggers"))]
    pub(super)  &'static str,
);

type Result<T> = std::result::Result<T, Invalid>;

fn fail<T>(why: &'static str) -> Result<T> {
    Err(Invalid(why))
}

/// One translated module and the contract its linker fulfils.
pub(super) struct Translation {
    /// The WebAssembly binary.
    pub(super) wasm: Vec<u8>,
    /// `foreign` members the module reads, in declaration order.
    pub(super) foreign: Vec<ForeignMember>,
    /// Imports in wasm order.
    pub(super) imports: Vec<Import>,
    /// Standard-library members the module captured, to check at link.
    pub(super) stdlib: Vec<StdlibUse>,
    /// The module views the heap, its memory `0`, which the linker's memory
    /// creator backs with the heap buffer.
    pub(super) memory: bool,
    /// The module's return value.
    pub(super) exports: Exports,
    /// Display name and arity per exported wasm function, keyed by the wasm
    /// export name.
    pub(super) functions: Vec<ExportedFunction>,
}

/// A `foreign.<name>` module variable.
pub(super) struct ForeignMember {
    pub(super) name: String,
    pub(super) kind: ForeignKind,
}

/// How a module variable reads its `foreign` member.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ForeignKind {
    /// `foreign.<name> | 0`.
    Int,
    /// `+foreign.<name>`.
    Double,
    /// `foreign.<name>`: a function the module calls.
    Function,
}

/// One wasm import the linker supplies.
pub(super) enum Import {
    /// The value of foreign member `foreign` (an `Int` or `Double` read).
    Value { foreign: u32, double: bool },
    /// Foreign member `foreign` called with these wasm parameter types and
    /// result (`None` for a call in statement position).
    Function {
        foreign: u32,
        params: Vec<ValType>,
        result: Option<ValType>,
    },
    /// A transcendental `Math` member, computed by the shared builtin
    /// arithmetic: `(f64) -> f64` or `(f64, f64) -> f64`.
    Math(StdlibFunction),
    /// `%` on doubles (JavaScript's `fmod`).
    Fmod,
}

/// A standard-library member the module captured.
pub(super) enum StdlibUse {
    /// `stdlib.Math.<f>` or `new stdlib.<View>(heap)`, by builtin identity.
    Function(StdlibFunction),
    /// `stdlib.Math.<name>`, which must still hold `value`.
    MathConstant(&'static str, f64),
    /// `stdlib.Infinity`.
    Infinity,
    /// `stdlib.NaN`.
    NaN,
}

/// The module's return value.
pub(super) enum Exports {
    /// `return f`: the wasm export named here.
    Single(String),
    /// `return {name: f, ...}`: `(property, wasm export)` in source order.
    Object(Vec<(String, String)>),
}

/// An exported wasm function's JavaScript face.
pub(super) struct ExportedFunction {
    pub(super) export: String,
    pub(super) name: String,
    pub(super) arity: u8,
}

/// The heap a module is translated against.
#[derive(Clone, Copy)]
pub(super) struct Heap {
    /// Byte length.
    pub(super) len: u32,
    /// The bytes start a reservation that reads as zero up to 4 GiB
    /// (`otter_runtime::byte_storage`): integer loads need no check, and
    /// the engine elides its own.
    pub(super) reserved: bool,
}

/// Translate the `"use asm"` function whose [[SourceText]] is `source`
/// against `heap` (`None` without a usable heap).
pub(super) fn translate(source: &str, heap: Option<Heap>) -> Result<Translation> {
    let wrapped = format!("({source})");
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &wrapped, SourceType::cjs())
        .with_options(ParseOptions {
            preserve_parens: true,
            ..ParseOptions::default()
        })
        .parse();
    if !parsed.diagnostics.is_empty() || parsed.program.body.len() != 1 {
        return fail("module source does not parse");
    }
    let Statement::ExpressionStatement(statement) = &parsed.program.body[0] else {
        return fail("module source is not a function");
    };
    let mut expression = &statement.expression;
    while let Expression::ParenthesizedExpression(inner) = expression {
        expression = &inner.expression;
    }
    let Expression::FunctionExpression(function) = expression else {
        return fail("module is not a function expression");
    };
    let mut module = ModuleTranslator::new(heap);
    module.module(function)?;
    module.finish()
}

// ---------------------------------------------------------------------------
// Module state
// ---------------------------------------------------------------------------

/// What a module-scope name denotes.
#[derive(Clone, Copy)]
enum Binding {
    /// A wasm global defined by the module (index past the imported ones).
    Global {
        ty: AsmType,
        slot: u32,
        mutable: bool,
    },
    /// A `stdlib.Math` / `stdlib` number constant.
    Constant(f64),
    /// A heap view.
    View(HeapView),
    /// A standard-library function.
    Stdlib(StdlibFunction),
    /// A foreign function, by foreign member index.
    Foreign(u32),
    /// An asm.js function, by declaration index.
    Function(u32),
    /// A function table, by table index.
    Table(u32),
}

struct FunctionInfo<'a> {
    name: &'a str,
    signature: Option<Signature>,
}

struct TableInfo {
    mask: u32,
    base: u32,
    signature: Option<Signature>,
}

/// A symbolic call target, resolved when bodies are encoded.
#[derive(Clone, Copy)]
enum Callee {
    Import(u32),
    Defined(u32),
}

/// One buffered instruction.
enum Op {
    Wasm(Instruction<'static>),
    Call(Callee),
}

/// The out-of-line half of `~~double`: JavaScript `ToInt32` of a double that
/// is NaN, infinite or at least 2^63 in magnitude.
const TO_INT32_SLOW: u32 = 0;
/// Helpers emitted after the asm.js functions.
const HELPER_COUNT: u32 = 1;

struct ModuleTranslator<'a> {
    heap: Option<Heap>,
    stdlib_name: Option<&'a str>,
    foreign_name: Option<&'a str>,
    heap_name: Option<&'a str>,
    names: HashMap<&'a str, Binding>,
    types: Vec<(Vec<ValType>, Vec<ValType>)>,
    type_index: HashMap<(Vec<ValType>, Vec<ValType>), u32>,
    foreign: Vec<ForeignMember>,
    /// Imported globals: `(foreign member, wasm type)`.
    global_imports: Vec<(u32, ValType)>,
    /// Defined globals: `(wasm type, init)`.
    globals: Vec<(ValType, ConstExpr)>,
    /// Imported functions: `(import, type index)`.
    function_imports: Vec<(Import, u32)>,
    foreign_imports: HashMap<(u32, Vec<ValType>, Option<ValType>), u32>,
    math_imports: HashMap<&'static str, u32>,
    fmod_import: Option<u32>,
    functions: Vec<FunctionInfo<'a>>,
    bodies: Vec<Option<(Vec<ValType>, Vec<Op>)>>,
    tables: Vec<TableInfo>,
    table_elements: Vec<u32>,
    stdlib: Vec<StdlibUse>,
    memory: bool,
    exports: Option<Exports>,
}

impl<'a> ModuleTranslator<'a> {
    fn new(heap: Option<Heap>) -> Self {
        Self {
            heap,
            stdlib_name: None,
            foreign_name: None,
            heap_name: None,
            names: HashMap::new(),
            types: Vec::new(),
            type_index: HashMap::new(),
            foreign: Vec::new(),
            global_imports: Vec::new(),
            globals: Vec::new(),
            function_imports: Vec::new(),
            foreign_imports: HashMap::new(),
            math_imports: HashMap::new(),
            fmod_import: None,
            functions: Vec::new(),
            bodies: Vec::new(),
            tables: Vec::new(),
            table_elements: Vec::new(),
            stdlib: Vec::new(),
            memory: false,
            exports: None,
        }
    }

    fn type_of(&mut self, params: Vec<ValType>, results: Vec<ValType>) -> u32 {
        let key = (params, results);
        if let Some(&index) = self.type_index.get(&key) {
            return index;
        }
        let index = self.types.len() as u32;
        self.types.push(key.clone());
        self.type_index.insert(key, index);
        index
    }

    fn signature_type(&mut self, signature: &Signature) -> u32 {
        self.type_of(signature.wasm_params(), signature.wasm_results())
    }

    fn import_function(
        &mut self,
        import: Import,
        params: Vec<ValType>,
        results: Vec<ValType>,
    ) -> u32 {
        let ty = self.type_of(params, results);
        self.function_imports.push((import, ty));
        self.function_imports.len() as u32 - 1
    }

    fn math_import(&mut self, function: StdlibFunction, name: &'static str, arity: usize) -> u32 {
        if let Some(&index) = self.math_imports.get(name) {
            return index;
        }
        let index = self.import_function(
            Import::Math(function),
            vec![ValType::F64; arity],
            vec![ValType::F64],
        );
        self.math_imports.insert(name, index);
        index
    }

    fn fmod_import(&mut self) -> u32 {
        if let Some(index) = self.fmod_import {
            return index;
        }
        let index = self.import_function(
            Import::Fmod,
            vec![ValType::F64, ValType::F64],
            vec![ValType::F64],
        );
        self.fmod_import = Some(index);
        index
    }

    fn foreign_import(
        &mut self,
        foreign: u32,
        params: Vec<ValType>,
        result: Option<ValType>,
    ) -> u32 {
        let key = (foreign, params.clone(), result);
        if let Some(&index) = self.foreign_imports.get(&key) {
            return index;
        }
        let index = self.import_function(
            Import::Function {
                foreign,
                params: params.clone(),
                result,
            },
            params,
            result.into_iter().collect(),
        );
        self.foreign_imports.insert(key, index);
        index
    }

    fn define_global(&mut self, ty: ValType, init: ConstExpr) -> u32 {
        self.globals.push((ty, init));
        self.globals.len() as u32 - 1
    }

    /// Wasm index of the defined global in `slot`.
    fn global_index(&self, slot: u32) -> u32 {
        self.global_imports.len() as u32 + slot
    }

    fn declare(&mut self, name: &'a str, binding: Binding) -> Result<()> {
        if Some(name) == self.stdlib_name
            || Some(name) == self.foreign_name
            || Some(name) == self.heap_name
        {
            return fail("module name shadows a parameter");
        }
        if self.names.insert(name, binding).is_some() {
            return fail("module name redefined");
        }
        Ok(())
    }

    /// Last valid byte address plus one for a `size`-byte access.
    fn heap_limit(&self, size: u32) -> Result<u32> {
        match self.heap {
            Some(heap) => Ok(heap.len - (size - 1)),
            None => fail("heap access without a valid heap"),
        }
    }

    /// Integer loads past the heap read zero without a check.
    fn reads_zero_past_heap(&self) -> bool {
        self.heap.is_some_and(|heap| heap.reserved)
    }

    // -- 6.1 ValidateModule -------------------------------------------------

    fn module(&mut self, function: &'a Function<'a>) -> Result<()> {
        if function.generator || function.r#async {
            return fail("module is resumable");
        }
        let params = &function.params;
        if params.rest.is_some() || params.items.len() > 3 {
            return fail("bad module parameters");
        }
        let mut names = Vec::new();
        for param in &params.items {
            let BindingPattern::BindingIdentifier(id) = &param.pattern else {
                return fail("bad module parameter");
            };
            if param.initializer.is_some() || names.contains(&id.name.as_str()) {
                return fail("bad module parameter");
            }
            names.push(id.name.as_str());
        }
        self.stdlib_name = names.first().copied();
        self.foreign_name = names.get(1).copied();
        self.heap_name = names.get(2).copied();
        let Some(body) = &function.body else {
            return fail("module has no body");
        };
        if !body
            .directives
            .iter()
            .any(|directive| directive.directive.as_str() == "use asm")
        {
            return fail("missing \"use asm\"");
        }
        let statements = &body.statements;
        let mut at = 0;
        // Module variables.
        while let Some(Statement::VariableDeclaration(declaration)) = statements.get(at) {
            if is_function_table(declaration) {
                break;
            }
            self.module_variables(declaration)?;
            at += 1;
        }
        // Functions: registered before any body so calls may precede
        // definitions.
        let functions_start = at;
        while let Some(Statement::FunctionDeclaration(function)) = statements.get(at) {
            let Some(id) = &function.id else {
                return fail("function without a name");
            };
            let index = self.functions.len() as u32;
            self.functions.push(FunctionInfo {
                name: id.name.as_str(),
                signature: None,
            });
            self.bodies.push(None);
            self.declare(id.name.as_str(), Binding::Function(index))?;
            at += 1;
        }
        let functions_end = at;
        // Function tables.
        let tables_start = at;
        while let Some(Statement::VariableDeclaration(declaration)) = statements.get(at) {
            for declarator in &declaration.declarations {
                let (name, elements) = table_declarator(declaration, declarator)?;
                let len = elements.len() as u32;
                if !len.is_power_of_two() {
                    return fail("function table size is not a power of two");
                }
                let index = self.tables.len() as u32;
                let base = self.tables.iter().map(|table| table.mask + 1).sum();
                self.tables.push(TableInfo {
                    mask: len - 1,
                    base,
                    signature: None,
                });
                self.declare(name, Binding::Table(index))?;
            }
            at += 1;
        }
        let tables_end = at;
        let Some(Statement::ReturnStatement(ret)) = statements.get(at) else {
            return fail("module does not end in an export");
        };
        if statements.len() != at + 1 {
            return fail("statements after the export");
        }
        for (index, statement) in statements[functions_start..functions_end]
            .iter()
            .enumerate()
        {
            let Statement::FunctionDeclaration(function) = statement else {
                unreachable!("counted above");
            };
            self.function(index as u32, function)?;
        }
        for statement in &statements[tables_start..tables_end] {
            let Statement::VariableDeclaration(declaration) = statement else {
                unreachable!("counted above");
            };
            for declarator in &declaration.declarations {
                let (name, elements) = table_declarator(declaration, declarator)?;
                self.table_definition(name, &elements)?;
            }
        }
        self.exports(ret.argument.as_ref())
    }

    // -- 6.3 ValidateFunctionTable ------------------------------------------

    fn table_definition(&mut self, name: &'a str, elements: &[&'a str]) -> Result<()> {
        let Some(Binding::Table(table)) = self.names.get(name).copied() else {
            unreachable!("tables are declared before bodies");
        };
        let mut signature = self.tables[table as usize].signature.clone();
        for element in elements {
            let Some(Binding::Function(function)) = self.names.get(element).copied() else {
                return fail("function table element is not a function");
            };
            let Some(element_signature) = &self.functions[function as usize].signature else {
                return fail("function table element has no type");
            };
            match &signature {
                // An unused table takes its first element's type.
                None => signature = Some(element_signature.clone()),
                Some(signature) if signature == element_signature => {}
                Some(_) => return fail("function table definition doesn't match use"),
            }
            self.table_elements.push(HELPER_COUNT + function);
        }
        self.tables[table as usize].signature = signature;
        Ok(())
    }

    // -- 6.2 ValidateExport -------------------------------------------------

    fn exports(&mut self, argument: Option<&'a Expression<'a>>) -> Result<()> {
        let export_of = |this: &Self, expression: &Expression<'a>| -> Result<u32> {
            let Expression::Identifier(id) = expression else {
                return fail("export is not a function");
            };
            match this.names.get(id.name.as_str()) {
                Some(Binding::Function(index)) => Ok(*index),
                _ => fail("export is not a function"),
            }
        };
        match argument {
            Some(Expression::ObjectExpression(object)) => {
                let mut exports = Vec::new();
                for property in &object.properties {
                    let ObjectPropertyKind::ObjectProperty(property) = property else {
                        return fail("bad export");
                    };
                    if property.computed || property.shorthand || property.method {
                        return fail("bad export");
                    }
                    let PropertyKey::StaticIdentifier(key) = &property.key else {
                        return fail("bad export name");
                    };
                    let name = key.name.as_str();
                    if exports.iter().any(|(existing, _)| existing == name) {
                        return fail("duplicate export");
                    }
                    let function = export_of(self, &property.value)?;
                    exports.push((name.to_string(), function.to_string()));
                }
                self.exports = Some(Exports::Object(exports));
            }
            Some(expression @ Expression::Identifier(_)) => {
                let function = export_of(self, expression)?;
                self.exports = Some(Exports::Single(function.to_string()));
            }
            _ => return fail("bad export"),
        }
        Ok(())
    }

    // -- 6.4 ValidateFunction -----------------------------------------------

    fn function(&mut self, index: u32, function: &'a Function<'a>) -> Result<()> {
        if function.generator || function.r#async || function.params.rest.is_some() {
            return fail("bad function");
        }
        let Some(body) = &function.body else {
            return fail("function without body");
        };
        if !body.directives.is_empty() {
            return fail("directives inside an asm.js function");
        }
        let mut params = Vec::new();
        for param in &function.params.items {
            let BindingPattern::BindingIdentifier(id) = &param.pattern else {
                return fail("bad parameter");
            };
            if param.initializer.is_some() {
                return fail("bad parameter");
            }
            params.push(id.name.as_str());
        }
        let mut cx = FunctionTranslator::new(self, params.len() as u32);
        let statements = &body.statements;
        let mut at = 0;
        // 5.1 Parameter type annotations, one per parameter, in order.
        let mut param_types = Vec::new();
        for name in &params {
            let Some(Statement::ExpressionStatement(statement)) = statements.get(at) else {
                return fail("missing parameter annotation");
            };
            let ty = cx.parameter_annotation(name, &statement.expression)?;
            cx.declare_local(name, ty, param_types.len() as u32)?;
            param_types.push(ty);
            at += 1;
        }
        // Local variables.
        while let Some(Statement::VariableDeclaration(declaration)) = statements.get(at) {
            cx.local_variables(declaration)?;
            at += 1;
        }
        let mut last_is_return = false;
        for statement in &statements[at..] {
            last_is_return = matches!(statement, Statement::ReturnStatement(_));
            cx.statement(statement)?;
        }
        let result = match cx.return_type {
            None => AsmType::VOID,
            Some(ty) if ty == AsmType::VOID => AsmType::VOID,
            Some(ty) if last_is_return => ty,
            Some(_) => return fail("expected return at end of non-void function"),
        };
        let (locals, ops) = cx.finish();
        let signature = Signature {
            params: param_types,
            result,
        };
        let info = &mut self.functions[index as usize];
        match &info.signature {
            None => info.signature = Some(signature),
            Some(expected) if *expected == signature => {}
            Some(_) => return fail("function definition doesn't match use"),
        }
        self.bodies[index as usize] = Some((locals, ops));
        Ok(())
    }
}
