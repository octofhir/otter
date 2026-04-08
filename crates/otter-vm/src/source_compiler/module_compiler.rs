use super::ast::{ParamInfo, has_use_strict_directive};
use super::shared::{CompiledFunction, FunctionCompiler, FunctionKind, ScopeRef};
use super::source_mapper::SourceMapper;
use super::*;

use crate::module::{ExportRecord, ImportRecord};

#[derive(Debug, Clone, Default)]
pub(super) struct FunctionIdentity {
    pub(super) debug_name: Option<String>,
    pub(super) self_binding_name: Option<String>,
    pub(super) length: u16,
}

pub(super) struct ModuleCompiler<'a> {
    source_url: &'a str,
    mode: LoweringMode,
    functions: Vec<Option<VmFunction>>,
    /// §16.2.2 — Import records collected during module compilation.
    imports: Vec<ImportRecord>,
    /// §16.2.3 — Export records collected during module compilation.
    exports: Vec<ExportRecord>,
    /// Shared source mapper, cloned into every `FunctionCompiler` so they
    /// can resolve AST spans to 1-based `(line, column)` in the **original**
    /// source (TS or JS as written by the user).
    source_mapper: Rc<SourceMapper>,
    /// Original source text attached to the produced `Module` so runtime
    /// diagnostics can render snippets that match what the user wrote. For
    /// `.js` files this is the literal JS; for `.ts` files this is the
    /// literal TS (not the generated JS).
    original_source: Arc<str>,
}

impl<'a> ModuleCompiler<'a> {
    pub(super) fn new(
        source_url: &'a str,
        mode: LoweringMode,
        source_mapper: Rc<SourceMapper>,
        original_source: Arc<str>,
    ) -> Self {
        Self {
            source_url,
            mode,
            functions: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            source_mapper,
            original_source,
        }
    }

    /// Returns a handle to the shared source mapper for child
    /// `FunctionCompiler` creation.
    pub(super) fn source_mapper(&self) -> Rc<SourceMapper> {
        self.source_mapper.clone()
    }

    /// Adds an import record (used by import declaration compilation).
    pub(super) fn add_import(&mut self, record: ImportRecord) {
        self.imports.push(record);
    }

    /// Adds an export record (used by export declaration compilation).
    pub(super) fn add_export(&mut self, record: ExportRecord) {
        self.exports.push(record);
    }

    /// Returns the current lowering mode.
    pub(super) fn mode(&self) -> LoweringMode {
        self.mode
    }

    pub(super) fn compile(
        mut self,
        program: &AstProgram<'_>,
    ) -> Result<Module, SourceLoweringError> {
        let is_module = self.mode == LoweringMode::Module;
        let entry = self.reserve_function();
        // §10.2.1 — Module code is always strict.
        let inherited_strict = is_module || has_use_strict_directive(program.directives.as_slice());
        let compiled = self.compile_function_from_statements(
            entry,
            FunctionIdentity {
                debug_name: Some(self.source_url.to_string()),
                self_binding_name: None,
                length: 0,
            },
            &program.body,
            &[],
            FunctionKind::Script,
            Vec::new(),
            inherited_strict,
        )?;
        self.functions[entry.0 as usize] = Some(compiled.function);

        let functions = self
            .functions
            .into_iter()
            .enumerate()
            .map(|(index, function)| {
                function.ok_or_else(|| {
                    SourceLoweringError::Unsupported(format!(
                        "internal function slot {} was left undefined",
                        index
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let source_text = self.original_source.clone();
        if is_module {
            Module::new_esm(
                Some(self.source_url),
                functions,
                entry,
                self.imports,
                self.exports,
            )
            .map(|module| module.with_source_text(source_text))
            .map_err(|error| {
                SourceLoweringError::Unsupported(format!("failed to construct module: {error}"))
            })
        } else {
            Module::new(Some(self.source_url), functions, entry)
                .map(|module| module.with_source_text(source_text))
                .map_err(|error| {
                    SourceLoweringError::Unsupported(format!("failed to construct module: {error}"))
                })
        }
    }

    pub(super) fn reserve_function(&mut self) -> FunctionIndex {
        let index = FunctionIndex(self.functions.len() as u32);
        self.functions.push(None);
        index
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn compile_function_from_statements(
        &mut self,
        function_index: FunctionIndex,
        identity: FunctionIdentity,
        statements: &[AstStatement<'_>],
        params: &[ParamInfo<'_>],
        kind: FunctionKind,
        parent_scopes: Vec<ScopeRef>,
        inherited_strict: bool,
    ) -> Result<CompiledFunction, SourceLoweringError> {
        self.compile_function_from_statements_with_options(
            function_index,
            identity,
            statements,
            params,
            kind,
            parent_scopes,
            inherited_strict,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn compile_function_from_statements_with_options(
        &mut self,
        function_index: FunctionIndex,
        identity: FunctionIdentity,
        statements: &[AstStatement<'_>],
        params: &[ParamInfo<'_>],
        kind: FunctionKind,
        parent_scopes: Vec<ScopeRef>,
        inherited_strict: bool,
        is_derived_constructor: bool,
    ) -> Result<CompiledFunction, SourceLoweringError> {
        let mut compiler = FunctionCompiler::new(
            self.mode,
            identity.debug_name.clone(),
            kind,
            parent_scopes,
            self.source_mapper.clone(),
        );
        compiler.strict_mode = inherited_strict;
        compiler.is_derived_constructor = is_derived_constructor;

        compiler.declare_parameters(params)?;
        if kind != FunctionKind::Arrow {
            compiler.declare_this_binding()?;
        }
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(params, self)?;
        if kind == FunctionKind::Script {
            compiler.declare_intrinsic_globals()?;
        }
        if let Some(self_binding_name) = identity.self_binding_name.as_deref() {
            let closure_register = compiler.declare_function_binding(self_binding_name)?;
            compiler
                .instructions
                .push(Instruction::load_current_closure(closure_register));
        }
        compiler.predeclare_function_scope(statements, self)?;
        compiler.emit_hoisted_function_initializers()?;
        let terminated = compiler.compile_statements(statements, self)?;

        // §16.2.3 — In module mode, ensure all exported local bindings are
        // stored on the global object so the host can read them after evaluation.
        // `var` and hoisted function declarations already use SetGlobal, but
        // `const`/`let` and non-hoisted exports need explicit global writes.
        if self.mode == LoweringMode::Module {
            compiler.emit_module_export_globals(&self.exports)?;
        }

        if !terminated {
            compiler.emit_implicit_return()?;
        }

        compiler.finish(
            function_index,
            identity.length,
            identity.debug_name.as_deref(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn compile_function_from_expression(
        &mut self,
        function_index: FunctionIndex,
        identity: FunctionIdentity,
        expression: &Expression<'_>,
        params: &[ParamInfo<'_>],
        kind: FunctionKind,
        parent_scopes: Vec<ScopeRef>,
        inherited_strict: bool,
    ) -> Result<CompiledFunction, SourceLoweringError> {
        let mut compiler = FunctionCompiler::new(
            self.mode,
            identity.debug_name.clone(),
            kind,
            parent_scopes,
            self.source_mapper.clone(),
        );
        compiler.strict_mode = inherited_strict;

        compiler.declare_parameters(params)?;
        if kind != FunctionKind::Arrow {
            compiler.declare_this_binding()?;
        }
        compiler.reserve_arguments_binding_slot()?;
        compiler.compile_parameter_initialization(params, self)?;
        let value = compiler.compile_expression(expression, self)?;
        compiler.instructions.push(Instruction::ret(value.register));
        compiler.release(value);

        compiler.finish(
            function_index,
            identity.length,
            identity.debug_name.as_deref(),
        )
    }

    pub(super) fn set_function(&mut self, index: FunctionIndex, function: VmFunction) {
        self.functions[index.0 as usize] = Some(function);
    }
}
