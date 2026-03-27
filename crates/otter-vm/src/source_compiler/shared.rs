use super::*;

#[derive(Debug, Clone, Copy)]
pub(super) enum Binding {
    Register(BytecodeRegister),
    Function { closure_register: BytecodeRegister },
    Upvalue(UpvalueId),
}

impl Binding {
    pub(super) fn capture_source(self) -> CaptureSource {
        match self {
            Self::Register(register) => CaptureSource::Register(register),
            Self::Function {
                closure_register, ..
            } => CaptureSource::Register(closure_register),
            Self::Upvalue(upvalue) => CaptureSource::Upvalue(upvalue),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum CaptureSource {
    Register(BytecodeRegister),
    Upvalue(UpvalueId),
}

#[derive(Debug, Clone)]
pub(super) struct CompileEnv {
    pub(super) bindings: BTreeMap<String, Binding>,
}

impl CompileEnv {
    pub(super) fn new() -> Self {
        Self {
            bindings: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ValueLocation {
    pub(super) register: BytecodeRegister,
    pub(super) is_temp: bool,
}

impl ValueLocation {
    pub(super) const fn local(register: BytecodeRegister) -> Self {
        Self {
            register,
            is_temp: false,
        }
    }

    pub(super) const fn temp(register: BytecodeRegister) -> Self {
        Self {
            register,
            is_temp: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FunctionKind {
    Script,
    Ordinary,
    Arrow,
}

#[derive(Debug, Clone)]
pub(super) struct PendingFunction {
    pub(super) reserved: FunctionIndex,
    pub(super) closure_register: BytecodeRegister,
    pub(super) captures: Vec<CaptureSource>,
}

pub(super) struct CompiledFunction {
    pub(super) function: VmFunction,
    pub(super) captures: Vec<CaptureSource>,
}

#[derive(Debug, Clone)]
pub(super) struct FinallyScope {
    pub(super) return_flag_register: BytecodeRegister,
    pub(super) return_value_register: BytecodeRegister,
    pub(super) return_jumps: Vec<usize>,
}

#[derive(Debug, Clone)]
pub(super) struct LoopScope {
    pub(super) continue_target: Option<usize>,
    pub(super) break_jumps: Vec<usize>,
    pub(super) continue_jumps: Vec<usize>,
    pub(super) iterator_register: Option<BytecodeRegister>,
    pub(super) label: Option<String>,
}

pub(super) struct FunctionCompiler<'a> {
    pub(super) mode: LoweringMode,
    pub(super) function_name: Option<String>,
    pub(super) kind: FunctionKind,
    pub(super) parent_env: Option<CompileEnv>,
    pub(super) env: CompileEnv,
    pub(super) next_local: RegisterIndex,
    pub(super) parameter_count: RegisterIndex,
    pub(super) next_temp: RegisterIndex,
    pub(super) max_temp: RegisterIndex,
    pub(super) instructions: Vec<Instruction>,
    pub(super) property_names: Vec<Box<str>>,
    pub(super) property_name_ids: BTreeMap<String, PropertyNameId>,
    pub(super) string_literals: Vec<Box<str>>,
    pub(super) string_ids: BTreeMap<String, StringId>,
    pub(super) float_constants: Vec<f64>,
    pub(super) closure_templates: Vec<Option<ClosureTemplate>>,
    pub(super) call_sites: Vec<Option<CallSite>>,
    pub(super) exception_handlers: Vec<ExceptionHandler>,
    pub(super) captures: Vec<CaptureSource>,
    pub(super) capture_ids: BTreeMap<String, UpvalueId>,
    pub(super) hoisted_functions: Vec<PendingFunction>,
    pub(super) finally_stack: Vec<FinallyScope>,
    pub(super) loop_stack: Vec<LoopScope>,
    pub(super) pending_loop_label: Option<String>,
    /// ES2024 §10.4.4: Lazily allocated local for `arguments` object.
    /// `None` if `arguments` hasn't been referenced in this function body.
    pub(super) arguments_local: Option<crate::bytecode::BytecodeRegister>,
    pub(super) _marker: std::marker::PhantomData<&'a ()>,
}
