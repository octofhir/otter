use super::*;
use crate::closure::CaptureDescriptor;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug, Clone, Copy)]
pub(super) enum Binding {
    Register(BytecodeRegister),
    ThisRegister(BytecodeRegister),
    Function { closure_register: BytecodeRegister },
    Upvalue(UpvalueId),
    ThisUpvalue(UpvalueId),
}

impl Binding {
    pub(super) fn capture_source(self) -> CaptureSource {
        match self {
            Self::Register(register) => CaptureSource::Register(register),
            Self::ThisRegister(register) => CaptureSource::Register(register),
            Self::Function {
                closure_register, ..
            } => CaptureSource::Register(closure_register),
            Self::Upvalue(upvalue) => CaptureSource::Upvalue(upvalue),
            Self::ThisUpvalue(upvalue) => CaptureSource::Upvalue(upvalue),
        }
    }
}

pub(super) type CaptureSource = CaptureDescriptor;

/// One function-level scope frame, shared across nested function compilations
/// via `Rc<RefCell<>>` so that nested closures can materialize upvalues into
/// intermediate ancestor frames (per ES §9.1.2 GetIdentifierReference walking
/// the full scope chain).
#[derive(Debug)]
pub(super) struct ScopeFrame {
    /// Locally-visible bindings for this function (parameters, locals,
    /// implicit captures). Updated as compilation proceeds.
    pub(super) bindings: BTreeMap<String, Binding>,
    /// Captures the function will be constructed with — one entry per
    /// upvalue slot, in upvalue-id order.
    pub(super) captures: Vec<CaptureSource>,
    /// Map from name → upvalue id for already-captured names, used so the
    /// same outer name only consumes one upvalue slot.
    pub(super) capture_ids: BTreeMap<String, UpvalueId>,
}

impl ScopeFrame {
    pub(super) fn new() -> Self {
        Self {
            bindings: BTreeMap::new(),
            captures: Vec::new(),
            capture_ids: BTreeMap::new(),
        }
    }
}

pub(super) type ScopeRef = Rc<RefCell<ScopeFrame>>;

pub(super) fn new_scope_ref() -> ScopeRef {
    Rc::new(RefCell::new(ScopeFrame::new()))
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
    /// §27.3 Generator functions — `function*`.
    /// Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions>
    Generator,
    /// §27.7 Async functions — `async function`.
    /// Spec: <https://tc39.es/ecma262/#sec-async-function-definitions>
    Async,
    /// §27.7 Async arrow functions — `async () => {}`.
    AsyncArrow,
    /// §27.6 Async generator functions — `async function*`.
    /// Spec: <https://tc39.es/ecma262/#sec-async-generator-function-definitions>
    AsyncGenerator,
}

impl FunctionKind {
    pub(super) fn is_async(self) -> bool {
        matches!(self, Self::Async | Self::AsyncArrow | Self::AsyncGenerator)
    }

    pub(super) fn is_generator(self) -> bool {
        matches!(self, Self::Generator | Self::AsyncGenerator)
    }
}

#[derive(Debug, Clone)]
pub(super) struct PendingFunction {
    pub(super) reserved: FunctionIndex,
    pub(super) closure_register: BytecodeRegister,
    pub(super) captures: Vec<CaptureSource>,
    pub(super) is_generator: bool,
    pub(super) is_async: bool,
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
    pub(super) strict_mode: bool,
    pub(super) is_derived_constructor: bool,
    /// When true, this constructor has associated class instance fields.
    /// Causes `RunClassFieldInitializer` to be emitted at the right point.
    pub(super) has_instance_fields: bool,
    pub(super) function_name: Option<String>,
    pub(super) kind: FunctionKind,
    /// Ancestor scope frames, immediate parent first, outermost last.
    /// Cloned `Rc`s let `resolve_binding` walk the chain and materialize
    /// implicit upvalues at every intermediate level when a name is
    /// discovered several levels up.
    pub(super) parent_scopes: Vec<ScopeRef>,
    /// This function's own scope frame.
    pub(super) scope: ScopeRef,
    pub(super) next_local: RegisterIndex,
    pub(super) parameter_count: RegisterIndex,
    pub(super) next_temp: RegisterIndex,
    pub(super) max_temp: RegisterIndex,
    pub(super) instructions: Vec<Instruction>,
    pub(super) property_names: Vec<Box<str>>,
    pub(super) property_name_ids: BTreeMap<String, PropertyNameId>,
    pub(super) string_literals: Vec<crate::js_string::JsString>,
    pub(super) string_ids: BTreeMap<String, StringId>,
    pub(super) float_constants: Vec<f64>,
    pub(super) bigint_constants: Vec<Box<str>>,
    pub(super) bigint_ids: BTreeMap<String, crate::bigint::BigIntId>,
    pub(super) regexp_literals: Vec<(Box<str>, Box<str>)>,
    pub(super) regexp_ids: BTreeMap<(String, String), crate::regexp::RegExpId>,
    pub(super) closure_templates: Vec<Option<ClosureTemplate>>,
    pub(super) call_sites: Vec<Option<CallSite>>,
    pub(super) exception_handlers: Vec<ExceptionHandler>,
    pub(super) hoisted_functions: Vec<PendingFunction>,
    pub(super) finally_stack: Vec<FinallyScope>,
    pub(super) loop_stack: Vec<LoopScope>,
    pub(super) pending_loop_label: Option<String>,
    /// ES2024 §10.4.4: Lazily allocated local for `arguments` object.
    /// `None` if `arguments` hasn't been referenced in this function body.
    pub(super) arguments_local: Option<crate::bytecode::BytecodeRegister>,
    /// Local backing slot for a rest parameter array, when present.
    pub(super) rest_local: Option<crate::bytecode::BytecodeRegister>,
    /// Parameter or destructuring-binding locals that participate in parameter TDZ.
    pub(super) parameter_binding_registers: Vec<crate::bytecode::BytecodeRegister>,
    /// While true, reads of register-backed bindings must reject the internal hole sentinel.
    pub(super) parameter_tdz_active: bool,
    /// Top-level lexical names (`let`/`const`/`class` at the function body
    /// level) that were pre-declared during `predeclare_function_scope` so
    /// that hoisted nested functions can capture them via the closure scope
    /// chain. The actual `let foo = ...` statement re-uses the pre-allocated
    /// register slot rather than allocating a new one. Cleared once the slot
    /// has been claimed by the real declaration.
    pub(super) predeclared_lexical_names: std::collections::BTreeSet<String>,
    /// In eval mode, holds the register for the completion value of the last
    /// expression statement. Allocated lazily on the first expression statement.
    pub(super) eval_completion_register: Option<crate::bytecode::BytecodeRegister>,
    pub(super) _marker: std::marker::PhantomData<&'a ()>,
}
