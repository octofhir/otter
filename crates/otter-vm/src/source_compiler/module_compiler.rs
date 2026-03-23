use super::shared::{CompileEnv, CompiledFunction, FunctionCompiler, FunctionKind};
use super::*;

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
            Some(self.source_url.to_string()),
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
        name: Option<String>,
        statements: &[AstStatement<'_>],
        params: &[&str],
        kind: FunctionKind,
        parent_env: Option<CompileEnv>,
    ) -> Result<CompiledFunction, SourceLoweringError> {
        let mut compiler = FunctionCompiler::new(self.mode, name.clone(), kind, parent_env);

        compiler.declare_parameters(params)?;
        compiler.predeclare_function_scope(statements, self)?;
        compiler.emit_hoisted_function_initializers()?;
        let terminated = compiler.compile_statements(statements, self)?;
        if !terminated {
            compiler.emit_implicit_return()?;
        }

        compiler.finish(function_index, name.as_deref())
    }

    pub(super) fn set_function(&mut self, index: FunctionIndex, function: VmFunction) {
        self.functions[index.0 as usize] = Some(function);
    }
}
