use super::ast::ParamInfo;
use super::shared::{CompileEnv, CompiledFunction, FunctionCompiler, FunctionKind};
use super::*;

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
}

impl<'a> ModuleCompiler<'a> {
    pub(super) fn new(source_url: &'a str, mode: LoweringMode) -> Self {
        Self {
            source_url,
            mode,
            functions: Vec::new(),
        }
    }

    pub(super) fn compile(
        mut self,
        program: &AstProgram<'_>,
    ) -> Result<Module, SourceLoweringError> {
        let entry = self.reserve_function();
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
            None,
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

        Module::new(Some(self.source_url), functions, entry).map_err(|error| {
            SourceLoweringError::Unsupported(format!("failed to construct module: {error}"))
        })
    }

    pub(super) fn reserve_function(&mut self) -> FunctionIndex {
        let index = FunctionIndex(self.functions.len() as u32);
        self.functions.push(None);
        index
    }

    pub(super) fn compile_function_from_statements(
        &mut self,
        function_index: FunctionIndex,
        identity: FunctionIdentity,
        statements: &[AstStatement<'_>],
        params: &[ParamInfo<'_>],
        kind: FunctionKind,
        parent_env: Option<CompileEnv>,
    ) -> Result<CompiledFunction, SourceLoweringError> {
        let mut compiler =
            FunctionCompiler::new(self.mode, identity.debug_name.clone(), kind, parent_env);

        compiler.declare_parameters(params)?;
        if kind != FunctionKind::Arrow {
            compiler.declare_this_binding()?;
        }
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
        if !terminated {
            compiler.emit_implicit_return()?;
        }

        compiler.finish(
            function_index,
            identity.length,
            identity.debug_name.as_deref(),
        )
    }

    pub(super) fn compile_function_from_expression(
        &mut self,
        function_index: FunctionIndex,
        identity: FunctionIdentity,
        expression: &Expression<'_>,
        params: &[ParamInfo<'_>],
        kind: FunctionKind,
        parent_env: Option<CompileEnv>,
    ) -> Result<CompiledFunction, SourceLoweringError> {
        let mut compiler =
            FunctionCompiler::new(self.mode, identity.debug_name.clone(), kind, parent_env);

        compiler.declare_parameters(params)?;
        if kind != FunctionKind::Arrow {
            compiler.declare_this_binding()?;
        }
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
