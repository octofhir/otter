//! Mandatory structural verification for bytecode modules.
//!
//! The compiler, flat decoder, interpreter, and JIT all converge on the same
//! module admission proof. Verification is deterministic, side-effect free,
//! bounded by the admitted module, and never executes bytecode.
//!
//! # Contents
//! - [`verify_module`] — verify an ordinary base-zero compiler/cache module.
//! - [`verify_module_at_base`] — verify a module already rebased for a code
//!   space.
//! - [`VerifiedBytecodeModule`] — immutable admitted module plus retained
//!   per-function proofs.
//! - [`BytecodeVerifyError`] — typed rejection diagnostics.
//!
//! # Invariants
//! - Every function id is dense from the caller-supplied base.
//! - Wordcode storage, operand shapes, and control-flow targets are verified
//!   before any operand is decoded by a VM consumer.
//! - Register, constant, function, scope, and metadata indices are bounded by
//!   their owning tables. Every packed binding immediate decodes in its
//!   schema [`ImmediateDomain`]; context coordinates never name the reserved
//!   slot.
//! - Scope descriptors are well formed: a scope index and a slot index fit a
//!   context's `u16` fields, slot names are unique within a scope, and a strict
//!   scope never carries an eval extension.
//! - Context-register typing is not tracked. The verifier does not prove that
//!   a context operand holds a context, which scope it has, that a
//!   coordinate's `depth` stays on the chain, or that its `slot` is below the
//!   target scope's slot count; the VM validates each of those at run time and
//!   reports a typed error instead of reading out of bounds.
//! - Every context-held mapped formal names one parameter-context register,
//!   and `CollectArguments` / `CallForwardArguments` name that same register
//!   as their context operand.
//! - A derived-class constructor whose `this` is a `DerivedThis` context slot
//!   completes only through `ReturnDerived`, and `ReturnDerived` appears only
//!   there. A derived constructor with a frame-held `this` uses the ordinary
//!   return family.
//! - A [`VerifiedBytecodeModule`] exposes no mutable access to its module, and
//!   proof-preserving rebasing changes only function-id-bearing records.
//! - Successful verification does not publish runtime state.
//!
//! # See also
//! - [`crate::encoding::layout_wordcode_function`]
//! - [`crate::binary`]

use crate::encoding::{FunctionLayout, VerifyError, layout_wordcode_function};
use std::collections::HashSet;

use crate::opcode_schema::{ImmediateDomain, RegisterAccess, operand_spec_at, register_access_at};
use crate::{
    ArgumentBindingStorage, BytecodeModule, Constant, ContextCoord, Function, Op, Operand,
};

/// Constant-pool variant required by an opcode operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BytecodeConstantKind {
    /// WTF-16 string/property/name data.
    String,
    /// IEEE-754 number bits.
    Number,
    /// Function-table id.
    FunctionId,
    /// Decimal BigInt literal.
    BigInt,
    /// RegExp pattern and flags.
    RegExp,
}

impl BytecodeConstantKind {
    const fn of(constant: &Constant) -> Self {
        match constant {
            Constant::String { .. } => Self::String,
            Constant::Number { .. } => Self::Number,
            Constant::FunctionId { .. } => Self::FunctionId,
            Constant::BigInt { .. } => Self::BigInt,
            Constant::RegExp { .. } => Self::RegExp,
        }
    }
}

/// Retained proof for one function in a [`VerifiedBytecodeModule`].
///
/// The fields are private so consumers cannot manufacture a proof independently
/// of the module verifier. Execution builders borrow this record instead of
/// re-running wordcode or frame-window validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFunction {
    layout: FunctionLayout,
    register_count: u16,
}

impl VerifiedFunction {
    /// Canonical cold byte-PC layout established by wordcode verification.
    #[must_use]
    pub const fn layout(&self) -> &FunctionLayout {
        &self.layout
    }

    /// Exact register-window width established without saturating arithmetic.
    #[must_use]
    pub const fn register_count(&self) -> u16 {
        self.register_count
    }
}

/// An owned bytecode module that passed all mandatory admission checks.
///
/// The wrapped [`BytecodeModule`] is readable but cannot be mutated while the
/// proof is retained. This is the only type flat decoding returns, allowing a
/// cache to carry verification through VM linking and executable construction
/// without repeating it.
#[derive(Debug, Clone)]
pub struct VerifiedBytecodeModule {
    module: BytecodeModule,
    function_base: u32,
    functions: Box<[VerifiedFunction]>,
}

impl VerifiedBytecodeModule {
    /// Verify and own a normal compiler/cache module based at function id zero.
    ///
    /// # Errors
    /// Returns the first deterministic structural rejection.
    pub fn new(module: BytecodeModule) -> Result<Self, BytecodeVerifyError> {
        Self::new_at_base(module, 0)
    }

    /// Verify and own a module whose dense function ids start at `function_base`.
    ///
    /// # Errors
    /// Returns the first deterministic structural rejection.
    pub fn new_at_base(
        module: BytecodeModule,
        function_base: u32,
    ) -> Result<Self, BytecodeVerifyError> {
        let functions = verify_module_with_proofs(&module, function_base)?;
        Ok(Self {
            module,
            function_base,
            functions,
        })
    }

    /// Immutable access to the admitted module DTO.
    #[must_use]
    pub const fn module(&self) -> &BytecodeModule {
        &self.module
    }

    /// Dense function-id base proven for the wrapped module.
    #[must_use]
    pub const fn function_base(&self) -> u32 {
        self.function_base
    }

    /// Retained proof corresponding to one dense function-table index.
    #[must_use]
    pub fn function(&self, index: usize) -> Option<&VerifiedFunction> {
        self.functions.get(index)
    }

    /// Consume the proof and return the underlying module DTO.
    ///
    /// Once extracted, callers must verify the module again before executing it
    /// if they mutate it or do not preserve the proof by construction.
    #[must_use]
    pub fn into_module(self) -> BytecodeModule {
        self.module
    }

    /// Move this proof to a different dense function-id base without inspecting
    /// wordcode again.
    ///
    /// Every translated id was already proven to belong to the old dense range;
    /// layouts, frame windows, operands, and metadata are independent of the
    /// absolute base and therefore remain valid.
    ///
    /// # Errors
    /// Returns [`BytecodeRebaseError`] if the new dense range does not fit or
    /// if an internal function-id-bearing record no longer matches the retained
    /// proof.
    pub fn rebase_to(mut self, new_base: u32) -> Result<Self, BytecodeRebaseError> {
        if new_base == self.function_base {
            return Ok(self);
        }
        let function_count = u32::try_from(self.module.functions.len()).map_err(|_| {
            BytecodeRebaseError::FunctionRangeOverflow {
                base: new_base,
                function_count: self.module.functions.len(),
            }
        })?;
        new_base
            .checked_add(function_count)
            .ok_or(BytecodeRebaseError::FunctionRangeOverflow {
                base: new_base,
                function_count: self.module.functions.len(),
            })?;
        let old_base = self.function_base;
        let old_end = old_base.checked_add(function_count).ok_or(
            BytecodeRebaseError::FunctionRangeOverflow {
                base: old_base,
                function_count: self.module.functions.len(),
            },
        )?;

        for function in &mut self.module.functions {
            function.id = translate_verified_function_id(
                function.id,
                old_base,
                old_end,
                new_base,
                "function",
            )?;
            for site in &mut function.class_hint_sites {
                site.class_function_id = translate_verified_function_id(
                    site.class_function_id,
                    old_base,
                    old_end,
                    new_base,
                    "class hint",
                )?;
            }
        }
        for constant in &mut self.module.constants {
            if let Constant::FunctionId { index } = constant {
                *index = translate_verified_function_id(
                    *index, old_base, old_end, new_base, "constant",
                )?;
            }
        }
        for init in &mut self.module.module_inits {
            init.function_id = translate_verified_function_id(
                init.function_id,
                old_base,
                old_end,
                new_base,
                "module init",
            )?;
        }
        self.function_base = new_base;
        Ok(self)
    }
}

impl AsRef<BytecodeModule> for VerifiedBytecodeModule {
    fn as_ref(&self) -> &BytecodeModule {
        self.module()
    }
}

impl TryFrom<BytecodeModule> for VerifiedBytecodeModule {
    type Error = BytecodeVerifyError;

    fn try_from(module: BytecodeModule) -> Result<Self, Self::Error> {
        Self::new(module)
    }
}

/// Typed failure of proof-preserving absolute function-id rebasing.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BytecodeRebaseError {
    /// The new dense function-id range does not fit in `u32`.
    FunctionRangeOverflow {
        /// Requested first function id.
        base: u32,
        /// Function-table length.
        function_count: usize,
    },
    /// A supposedly verified record no longer belongs to its proven range.
    FunctionIdOutsideVerifiedRange {
        /// Record family being translated.
        record: &'static str,
        /// Encoded function id.
        function_id: u32,
        /// Inclusive proven first id.
        function_base: u32,
        /// Exclusive proven last id.
        function_end: u32,
    },
}

impl std::fmt::Display for BytecodeRebaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FunctionRangeOverflow {
                base,
                function_count,
            } => write!(
                f,
                "function range from base {base} with {function_count} entries exceeds u32"
            ),
            Self::FunctionIdOutsideVerifiedRange {
                record,
                function_id,
                function_base,
                function_end,
            } => write!(
                f,
                "verified {record} function id {function_id} is outside {function_base}..{function_end}"
            ),
        }
    }
}

impl std::error::Error for BytecodeRebaseError {}

fn translate_verified_function_id(
    function_id: u32,
    old_base: u32,
    old_end: u32,
    new_base: u32,
    record: &'static str,
) -> Result<u32, BytecodeRebaseError> {
    let local = function_id
        .checked_sub(old_base)
        .filter(|_| function_id < old_end);
    let Some(local) = local else {
        return Err(BytecodeRebaseError::FunctionIdOutsideVerifiedRange {
            record,
            function_id,
            function_base: old_base,
            function_end: old_end,
        });
    };
    new_base
        .checked_add(local)
        .ok_or(BytecodeRebaseError::FunctionRangeOverflow {
            base: new_base,
            function_count: usize::try_from(old_end - old_base).unwrap_or(usize::MAX),
        })
}

/// Typed reason a bytecode module was rejected before VM admission.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BytecodeVerifyError {
    /// Activation-window arguments reads require an unsuspended zero-formal function.
    ArgumentsReadMetadata {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
    },
    /// A module must contain `<main>` at function index zero.
    EmptyFunctionTable,
    /// Function count or its rebased end does not fit the u32 id space.
    FunctionRangeOverflow {
        /// Requested first function id.
        base: u32,
        /// Host function-table length.
        function_count: usize,
    },
    /// Function ids must equal `base + table_index` exactly.
    FunctionId {
        /// Dense table index.
        function_index: usize,
        /// Required global id.
        expected: u32,
        /// Encoded id.
        actual: u32,
    },
    /// Parameter/local/scratch counts do not fit the u16 register window.
    RegisterWindowOverflow {
        /// Owning function table index.
        function_index: usize,
        /// Encoded parameter count.
        parameters: u16,
        /// Encoded local count.
        locals: u16,
        /// Encoded scratch count.
        scratch: u16,
    },
    /// Wordcode storage, shape, or control flow is invalid.
    Wordcode {
        /// Owning function table index.
        function_index: usize,
        /// Exact wordcode error.
        error: VerifyError,
    },
    /// An operand could not be decoded after structural verification.
    OperandDecode {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Operand position.
        operand_index: usize,
    },
    /// A schema-declared register operand falls outside the frame window.
    RegisterOperand {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Opcode at the site.
        op: Op,
        /// Operand position.
        operand_index: usize,
        /// Signed decoded register number.
        register: i64,
        /// Exact register-window size.
        register_count: u32,
    },
    /// A packed binding immediate does not decode in its schema domain.
    ImmediateOperand {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Opcode at the site.
        op: Op,
        /// Operand position.
        operand_index: usize,
        /// Encoded immediate.
        value: i32,
        /// Schema domain the immediate must decode in.
        domain: ImmediateDomain,
    },
    /// `ReturnDerived` completes a function without a `DerivedThis` context
    /// slot, or a plain return completes one that has it. Such a derived
    /// constructor's completion reads its raw `this` explicitly, so every one
    /// of its returns is `ReturnDerived`.
    DerivedConstructorReturn {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// The offending return opcode.
        op: Op,
    },
    /// `CreateContext` names a scope outside the function's scope table.
    ContextScope {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Encoded scope index.
        scope: i32,
        /// Function scope-table length.
        scope_count: usize,
    },
    /// A scope descriptor violates a structural invariant.
    ScopeDescriptor {
        /// Owning function table index.
        function_index: usize,
        /// Scope-table index (the table length for a table-wide defect).
        scope_index: usize,
        /// The violated invariant.
        defect: ScopeDescriptorDefect,
    },
    /// `GetTemplateObject` points outside the module's template-site table.
    TemplateIndex {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Encoded template-site index.
        template_index: u32,
        /// Module template-site count.
        template_count: usize,
    },
    /// A constant-pool operand points outside the module pool.
    ConstantIndex {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Opcode at the site.
        op: Op,
        /// Operand position.
        operand_index: usize,
        /// Encoded constant index.
        constant_index: u32,
        /// Module constant count.
        constant_count: usize,
    },
    /// A constant-pool entry has the wrong semantic variant for its opcode.
    ConstantKind {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Opcode at the site.
        op: Op,
        /// Operand position.
        operand_index: usize,
        /// Encoded constant index.
        constant_index: u32,
        /// Required pool variant.
        expected: BytecodeConstantKind,
        /// Actual pool variant.
        actual: BytecodeConstantKind,
    },
    /// A function-id constant points outside this module's function range.
    FunctionConstant {
        /// Constant-pool index.
        constant_index: usize,
        /// Encoded function id.
        function_id: u32,
        /// Inclusive first valid id.
        function_base: u32,
        /// Exclusive last valid id.
        function_end: u32,
    },
    /// A metadata PC is not an instruction in its owning body.
    MetadataPc {
        /// Owning function table index.
        function_index: usize,
        /// Metadata family.
        metadata: &'static str,
        /// Encoded logical PC.
        pc: u32,
        /// Instruction count.
        instruction_count: usize,
    },
    /// Source-map entries must be sorted by logical PC.
    UnsortedSpans {
        /// Owning function table index.
        function_index: usize,
        /// Entry whose PC moved backwards.
        span_index: usize,
        /// Previous PC.
        previous_pc: u32,
        /// Current PC.
        pc: u32,
    },
    /// A source span has its end before its start.
    InvalidSourceSpan {
        /// Owning function table index.
        function_index: usize,
        /// Span family.
        metadata: &'static str,
        /// Encoded start.
        start: u32,
        /// Encoded end.
        end: u32,
    },
    /// An immediate-right operator names one register as both its destination
    /// and its left operand.
    ///
    /// Consumers are free to materialize the immediate into the destination
    /// register — that is what lets the form need no extra register — so an
    /// aliased destination would destroy the left operand before the operator
    /// reads it. The interpreter reads before it writes and would survive it;
    /// generated code does not, so the two would disagree.
    ImmediateOperandAliasesDestination {
        /// Index of the offending function in the module table.
        function_index: usize,
        /// Logical PC of the offending instruction.
        instruction_pc: usize,
        /// The immediate-right opcode.
        op: Op,
        /// The register named as both destination and left operand.
        register: u16,
    },
    /// A class-hint target points outside this module's function range.
    ClassHintFunction {
        /// Owning function table index.
        function_index: usize,
        /// Class-hint vector index.
        hint_index: usize,
        /// Encoded function id.
        function_id: u32,
        /// Inclusive first valid id.
        function_base: u32,
        /// Exclusive last valid id.
        function_end: u32,
    },
    /// Mapped-arguments metadata points outside parameters, registers, or
    /// addressable context slots.
    MappedArgument {
        /// Owning function table index.
        function_index: usize,
        /// Binding vector index.
        binding_index: usize,
        /// Invalid metadata field.
        field: &'static str,
        /// Encoded index.
        index: u32,
        /// Exclusive bound.
        limit: u32,
    },
    /// Context-held mapped formals name more than one parameter-context
    /// register, or a `CollectArguments` / `CallForwardArguments` site names a
    /// different one.
    MappedArgumentContext {
        /// Owning function table index.
        function_index: usize,
        /// Logical PC of the `CollectArguments` / `CallForwardArguments` site,
        /// or `None` for a
        /// disagreement inside the mapped-arguments table.
        instruction_pc: Option<usize>,
        /// Register named at the offending site.
        register: u16,
        /// Register the function's first context-held mapped formal names.
        expected: u16,
    },
    /// A tagged-template site has different cooked and raw arity.
    TemplateArity {
        /// Template-site index.
        site_index: usize,
        /// Cooked segment count.
        cooked: usize,
        /// Raw segment count.
        raw: usize,
    },
    /// A module-init record points outside this module's function range.
    ModuleInitFunction {
        /// Module-init vector index.
        init_index: usize,
        /// Encoded function id.
        function_id: u32,
        /// Inclusive first valid id.
        function_base: u32,
        /// Exclusive last valid id.
        function_end: u32,
    },
}

impl std::fmt::Display for BytecodeVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ArgumentsReadMetadata {
                function_index,
                instruction_pc,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} arguments read requires zero formals, arguments metadata, and no rest, eval or suspension"
            ),
            Self::EmptyFunctionTable => write!(f, "bytecode module has no <main> function"),
            Self::FunctionRangeOverflow {
                base,
                function_count,
            } => write!(
                f,
                "function range from base {base} with {function_count} entries exceeds u32"
            ),
            Self::FunctionId {
                function_index,
                expected,
                actual,
            } => write!(
                f,
                "function {function_index} has id {actual}, expected dense id {expected}"
            ),
            Self::RegisterWindowOverflow {
                function_index,
                parameters,
                locals,
                scratch,
            } => write!(
                f,
                "function {function_index} register window overflows u16: parameters={parameters} locals={locals} scratch={scratch}"
            ),
            Self::Wordcode {
                function_index,
                error,
            } => write!(f, "function {function_index} has invalid wordcode: {error}"),
            Self::OperandDecode {
                function_index,
                instruction_pc,
                operand_index,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} operand {operand_index} cannot be decoded"
            ),
            Self::RegisterOperand {
                function_index,
                instruction_pc,
                op,
                operand_index,
                register,
                register_count,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} {op:?} operand {operand_index} register {register} is outside 0..{register_count}"
            ),
            Self::ImmediateOperand {
                function_index,
                instruction_pc,
                op,
                operand_index,
                value,
                domain,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} {op:?} operand {operand_index} immediate {value:#x} is not a {domain:?}"
            ),
            Self::DerivedConstructorReturn {
                function_index,
                instruction_pc,
                op: Op::ReturnDerived,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} ReturnDerived outside a derived-class constructor with a DerivedThis slot"
            ),
            Self::DerivedConstructorReturn {
                function_index,
                instruction_pc,
                op,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} {op:?} completes a derived-class constructor with a DerivedThis slot, which only ReturnDerived may"
            ),
            Self::ContextScope {
                function_index,
                instruction_pc,
                scope,
                scope_count,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} CreateContext scope {scope} is outside 0..{scope_count}"
            ),
            Self::ScopeDescriptor {
                function_index,
                scope_index,
                defect,
            } => write!(
                f,
                "function {function_index} scope {scope_index} is malformed: {defect}"
            ),
            Self::TemplateIndex {
                function_index,
                instruction_pc,
                template_index,
                template_count,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} template site {template_index} is outside {template_count} entries"
            ),
            Self::ImmediateOperandAliasesDestination {
                function_index,
                instruction_pc,
                op,
                register,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} {op:?} names r{register} as both destination and left operand"
            ),
            Self::ConstantIndex {
                function_index,
                instruction_pc,
                op,
                operand_index,
                constant_index,
                constant_count,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} {op:?} operand {operand_index} constant {constant_index} is outside {constant_count} entries"
            ),
            Self::ConstantKind {
                function_index,
                instruction_pc,
                op,
                operand_index,
                constant_index,
                expected,
                actual,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} {op:?} operand {operand_index} constant {constant_index} is {actual:?}, expected {expected:?}"
            ),
            Self::FunctionConstant {
                constant_index,
                function_id,
                function_base,
                function_end,
            } => write!(
                f,
                "constant {constant_index} function id {function_id} is outside {function_base}..{function_end}"
            ),
            Self::MetadataPc {
                function_index,
                metadata,
                pc,
                instruction_count,
            } => write!(
                f,
                "function {function_index} {metadata} pc {pc} is outside {instruction_count} instructions"
            ),
            Self::UnsortedSpans {
                function_index,
                span_index,
                previous_pc,
                pc,
            } => write!(
                f,
                "function {function_index} span {span_index} pc {pc} follows larger pc {previous_pc}"
            ),
            Self::InvalidSourceSpan {
                function_index,
                metadata,
                start,
                end,
            } => write!(
                f,
                "function {function_index} {metadata} span {start}..{end} is reversed"
            ),
            Self::ClassHintFunction {
                function_index,
                hint_index,
                function_id,
                function_base,
                function_end,
            } => write!(
                f,
                "function {function_index} class hint {hint_index} target {function_id} is outside {function_base}..{function_end}"
            ),
            Self::MappedArgument {
                function_index,
                binding_index,
                field,
                index,
                limit,
            } => write!(
                f,
                "function {function_index} mapped argument {binding_index} {field} index {index} is outside 0..{limit}"
            ),
            Self::MappedArgumentContext {
                function_index,
                instruction_pc: Some(instruction_pc),
                register,
                expected,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} names context r{register}, mapped formals live in r{expected}"
            ),
            Self::MappedArgumentContext {
                function_index,
                instruction_pc: None,
                register,
                expected,
            } => write!(
                f,
                "function {function_index} mapped formals name context registers r{expected} and r{register}"
            ),
            Self::TemplateArity {
                site_index,
                cooked,
                raw,
            } => write!(
                f,
                "template site {site_index} has {cooked} cooked segments but {raw} raw segments"
            ),
            Self::ModuleInitFunction {
                init_index,
                function_id,
                function_base,
                function_end,
            } => write!(
                f,
                "module init {init_index} function id {function_id} is outside {function_base}..{function_end}"
            ),
        }
    }
}

impl std::error::Error for BytecodeVerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Wordcode { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Structural defect of one scope descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScopeDescriptorDefect {
    /// The scope table is longer than a context's `u16` scope index can name.
    TooManyScopes {
        /// Scope-table length.
        scope_count: usize,
    },
    /// The scope has more slots than a [`ContextCoord`] can address.
    TooManySlots {
        /// Declared slot count.
        slot_count: usize,
    },
    /// A slot repeats the name of an earlier slot of the same scope.
    DuplicateSlotName {
        /// Index of the repeating slot.
        slot_index: usize,
    },
    /// A strict scope declares an eval extension, which only a sloppy direct
    /// eval can populate.
    StrictExtension,
}

impl std::fmt::Display for ScopeDescriptorDefect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyScopes { scope_count } => write!(
                f,
                "{scope_count} scopes exceed the {} a context can name",
                usize::from(u16::MAX) + 1
            ),
            Self::TooManySlots { slot_count } => write!(
                f,
                "{slot_count} slots exceed the {} a coordinate can address",
                usize::from(ContextCoord::MAX_SLOT) + 1
            ),
            Self::DuplicateSlotName { slot_index } => {
                write!(f, "slot {slot_index} repeats an earlier slot name")
            }
            Self::StrictExtension => write!(f, "a strict scope declares an eval extension"),
        }
    }
}

/// Reject an immediate-right operator that names one register as both its
/// destination and its left operand.
///
/// The immediate-right forms carry their constant in the instruction rather
/// than the constant pool, so a consumer may materialize it into the
/// destination register and then run the ordinary two-register operator. That
/// is sound only while the destination differs from the left operand. The
/// interpreter reads both operands before writing and tolerates the alias;
/// generated code does not, so admitting it would let the two consumers
/// disagree on an artifact both accepted.
fn verify_immediate_right_operands(
    function_index: usize,
    instruction_pc: usize,
    instruction: &crate::wordcode::Instruction,
    code: &crate::wordcode::FunctionCode,
) -> Result<(), BytecodeVerifyError> {
    if !matches!(
        instruction.op,
        Op::AddImm
            | Op::SubImm
            | Op::BitwiseAndImm
            | Op::LessThanImm
            | Op::EqualImm
            | Op::NotEqualImm
    ) {
        return Ok(());
    }
    let (Some(Operand::Register(dst)), Some(Operand::Register(lhs))) =
        (code.operand(instruction, 0), code.operand(instruction, 1))
    else {
        // Operand shapes are checked by the wordcode layout pass; anything
        // else here is reported there.
        return Ok(());
    };
    if dst == lhs {
        return Err(BytecodeVerifyError::ImmediateOperandAliasesDestination {
            function_index,
            instruction_pc,
            op: instruction.op,
            register: dst,
        });
    }
    Ok(())
}

/// Verify a normal compiler/cache module whose dense function ids start at 0.
///
/// # Errors
/// Returns a typed structural error without mutating `module`.
pub fn verify_module(module: &BytecodeModule) -> Result<(), BytecodeVerifyError> {
    verify_module_at_base(module, 0)
}

/// Verify a module whose dense function ids start at `expected_base`.
///
/// Snapshot chunks and already-rebased code-space modules use this entry point;
/// normal compiler and cache modules use [`verify_module`].
///
/// # Errors
/// Returns a typed structural error without mutating `module`.
pub fn verify_module_at_base(
    module: &BytecodeModule,
    expected_base: u32,
) -> Result<(), BytecodeVerifyError> {
    verify_module_with_proofs(module, expected_base).map(drop)
}

fn verify_module_with_proofs(
    module: &BytecodeModule,
    expected_base: u32,
) -> Result<Box<[VerifiedFunction]>, BytecodeVerifyError> {
    if module.functions.is_empty() {
        return Err(BytecodeVerifyError::EmptyFunctionTable);
    }
    let function_count = u32::try_from(module.functions.len()).map_err(|_| {
        BytecodeVerifyError::FunctionRangeOverflow {
            base: expected_base,
            function_count: module.functions.len(),
        }
    })?;
    let function_end = expected_base.checked_add(function_count).ok_or(
        BytecodeVerifyError::FunctionRangeOverflow {
            base: expected_base,
            function_count: module.functions.len(),
        },
    )?;

    for (site_index, site) in module.template_sites.iter().enumerate() {
        if site.cooked.len() != site.raw.len() {
            return Err(BytecodeVerifyError::TemplateArity {
                site_index,
                cooked: site.cooked.len(),
                raw: site.raw.len(),
            });
        }
    }
    for (constant_index, constant) in module.constants.iter().enumerate() {
        if let Constant::FunctionId { index } = constant {
            verify_function_id(*index, expected_base, function_end).map_err(|()| {
                BytecodeVerifyError::FunctionConstant {
                    constant_index,
                    function_id: *index,
                    function_base: expected_base,
                    function_end,
                }
            })?;
        }
    }
    for (init_index, init) in module.module_inits.iter().enumerate() {
        verify_function_id(init.function_id, expected_base, function_end).map_err(|()| {
            BytecodeVerifyError::ModuleInitFunction {
                init_index,
                function_id: init.function_id,
                function_base: expected_base,
                function_end,
            }
        })?;
    }
    let mut verified_functions = Vec::with_capacity(module.functions.len());
    for (function_index, function) in module.functions.iter().enumerate() {
        let expected = expected_base.checked_add(function_index as u32).ok_or(
            BytecodeVerifyError::FunctionRangeOverflow {
                base: expected_base,
                function_count: module.functions.len(),
            },
        )?;
        if function.id != expected {
            return Err(BytecodeVerifyError::FunctionId {
                function_index,
                expected,
                actual: function.id,
            });
        }
        verified_functions.push(verify_function(
            module,
            function,
            function_index,
            expected_base,
            function_end,
        )?);
    }
    Ok(verified_functions.into_boxed_slice())
}

fn verify_function(
    module: &BytecodeModule,
    function: &Function,
    function_index: usize,
    function_base: u32,
    function_end: u32,
) -> Result<VerifiedFunction, BytecodeVerifyError> {
    let register_count = u32::from(function.param_count)
        .checked_add(u32::from(function.locals))
        .and_then(|count| count.checked_add(u32::from(function.scratch)))
        .filter(|count| *count <= u32::from(u16::MAX))
        .ok_or(BytecodeVerifyError::RegisterWindowOverflow {
            function_index,
            parameters: function.param_count,
            locals: function.locals,
            scratch: function.scratch,
        })?;
    let layout = layout_wordcode_function(&function.code).map_err(|error| {
        BytecodeVerifyError::Wordcode {
            function_index,
            error,
        }
    })?;
    let instruction_count = function.code.len();
    let _ = u32::try_from(instruction_count).map_err(|_| BytecodeVerifyError::Wordcode {
        function_index,
        error: VerifyError::FunctionTooLarge,
    })?;

    verify_source_span(function_index, "function", function.span)?;
    if let Some(span) = function.source_text_span {
        verify_source_span(function_index, "source-text", span)?;
    }
    let mut previous_span_pc = None;
    for (span_index, span) in function.spans.iter().enumerate() {
        verify_metadata_pc(function_index, "source span", span.pc, instruction_count)?;
        verify_source_span(function_index, "source-map", span.span)?;
        if let Some(previous_pc) = previous_span_pc
            && span.pc < previous_pc
        {
            return Err(BytecodeVerifyError::UnsortedSpans {
                function_index,
                span_index,
                previous_pc,
                pc: span.pc,
            });
        }
        previous_span_pc = Some(span.pc);
    }
    for &pc in &function.number_hint_sites {
        verify_metadata_pc(function_index, "number hint", pc, instruction_count)?;
    }
    for (hint_index, hint) in function.class_hint_sites.iter().enumerate() {
        verify_metadata_pc(function_index, "class hint", hint.pc, instruction_count)?;
        verify_function_id(hint.class_function_id, function_base, function_end).map_err(|()| {
            BytecodeVerifyError::ClassHintFunction {
                function_index,
                hint_index,
                function_id: hint.class_function_id,
                function_base,
                function_end,
            }
        })?;
    }

    verify_scope_descriptors(function, function_index)?;
    let mapped_context = verify_mapped_arguments(function, function_index, register_count)?;

    for (instruction_pc, instruction) in function.code.iter().enumerate() {
        if matches!(
            instruction.op,
            Op::LoadArgumentsLength | Op::LoadArgumentsElement
        ) && (!function.needs_arguments
            || function.param_count != 0
            || function.has_rest
            || function.contains_direct_eval
            || function.is_async
            || function.is_generator
            || function.is_async_generator)
        {
            return Err(BytecodeVerifyError::ArgumentsReadMetadata {
                function_index,
                instruction_pc,
            });
        }
        verify_immediate_right_operands(
            function_index,
            instruction_pc,
            instruction,
            &function.code,
        )?;
        for operand_index in 0..instruction.operand_count() {
            let operand = function.code.operand(instruction, operand_index).ok_or(
                BytecodeVerifyError::OperandDecode {
                    function_index,
                    instruction_pc,
                    operand_index,
                },
            )?;
            if register_access_at(instruction.op, operand_index) != RegisterAccess::None {
                let register = match operand {
                    Operand::Register(register) => i64::from(register),
                    Operand::Imm32(register) => i64::from(register),
                    Operand::ConstIndex(register) => i64::from(register),
                };
                if register < 0 || register >= i64::from(register_count) {
                    return Err(BytecodeVerifyError::RegisterOperand {
                        function_index,
                        instruction_pc,
                        op: instruction.op,
                        operand_index,
                        register,
                        register_count,
                    });
                }
            }
            if let Some(domain) =
                operand_spec_at(instruction.op, operand_index).and_then(|spec| spec.imm_domain)
            {
                verify_immediate_operand(
                    function,
                    function_index,
                    instruction_pc,
                    operand_index,
                    operand,
                    domain,
                )?;
            }
            if instruction.op.is_const_pool_operand(operand_index) {
                let Operand::ConstIndex(constant_index) = operand else {
                    return Err(BytecodeVerifyError::OperandDecode {
                        function_index,
                        instruction_pc,
                        operand_index,
                    });
                };
                let constant = module.constants.get(constant_index as usize).ok_or(
                    BytecodeVerifyError::ConstantIndex {
                        function_index,
                        instruction_pc,
                        op: instruction.op,
                        operand_index,
                        constant_index,
                        constant_count: module.constants.len(),
                    },
                )?;
                let expected = expected_constant_kind(instruction.op);
                let actual = BytecodeConstantKind::of(constant);
                if actual != expected {
                    return Err(BytecodeVerifyError::ConstantKind {
                        function_index,
                        instruction_pc,
                        op: instruction.op,
                        operand_index,
                        constant_index,
                        expected,
                        actual,
                    });
                }
            }
        }
        verify_instruction_semantics(
            module,
            function,
            function_index,
            instruction_pc,
            mapped_context,
        )?;
    }
    Ok(VerifiedFunction {
        layout,
        register_count: register_count as u16,
    })
}

/// Check the function's scope table: sizes fit a context's `u16` fields,
/// slot names are unique per scope, and no strict scope carries an eval
/// extension.
fn verify_scope_descriptors(
    function: &Function,
    function_index: usize,
) -> Result<(), BytecodeVerifyError> {
    if function.scopes.len() > usize::from(u16::MAX) + 1 {
        return Err(BytecodeVerifyError::ScopeDescriptor {
            function_index,
            scope_index: function.scopes.len(),
            defect: ScopeDescriptorDefect::TooManyScopes {
                scope_count: function.scopes.len(),
            },
        });
    }
    for (scope_index, scope) in function.scopes.iter().enumerate() {
        let defect = |defect| BytecodeVerifyError::ScopeDescriptor {
            function_index,
            scope_index,
            defect,
        };
        if scope.slots.len() > usize::from(ContextCoord::MAX_SLOT) + 1 {
            return Err(defect(ScopeDescriptorDefect::TooManySlots {
                slot_count: scope.slots.len(),
            }));
        }
        if scope.flags.strict && scope.flags.has_extension {
            return Err(defect(ScopeDescriptorDefect::StrictExtension));
        }
        let mut names = HashSet::with_capacity(scope.slots.len());
        for (slot_index, slot) in scope.slots.iter().enumerate() {
            if !names.insert(slot.name.as_str()) {
                return Err(defect(ScopeDescriptorDefect::DuplicateSlotName {
                    slot_index,
                }));
            }
        }
    }
    Ok(())
}

/// Bound the mapped-arguments table and return the one register every
/// context-held mapped formal names, if any.
fn verify_mapped_arguments(
    function: &Function,
    function_index: usize,
    register_count: u32,
) -> Result<Option<u16>, BytecodeVerifyError> {
    let mut mapped_context = None;
    for (binding_index, binding) in function.mapped_argument_bindings.iter().enumerate() {
        let out_of_range = |field, index, limit| BytecodeVerifyError::MappedArgument {
            function_index,
            binding_index,
            field,
            index,
            limit,
        };
        if u32::from(binding.argument_index) >= u32::from(function.param_count) {
            return Err(out_of_range(
                "argument",
                u32::from(binding.argument_index),
                u32::from(function.param_count),
            ));
        }
        match binding.storage {
            ArgumentBindingStorage::Register { reg } => {
                if u32::from(reg) >= register_count {
                    return Err(out_of_range("register", u32::from(reg), register_count));
                }
            }
            ArgumentBindingStorage::Context { reg, slot } => {
                if u32::from(reg) >= register_count {
                    return Err(out_of_range(
                        "context register",
                        u32::from(reg),
                        register_count,
                    ));
                }
                if slot > ContextCoord::MAX_SLOT {
                    return Err(out_of_range(
                        "context slot",
                        u32::from(slot),
                        u32::from(ContextCoord::MAX_SLOT) + 1,
                    ));
                }
                match mapped_context {
                    Some(expected) if expected != reg => {
                        return Err(BytecodeVerifyError::MappedArgumentContext {
                            function_index,
                            instruction_pc: None,
                            register: reg,
                            expected,
                        });
                    }
                    _ => mapped_context = Some(reg),
                }
            }
        }
    }
    Ok(mapped_context)
}

/// Admit one packed binding immediate against its schema domain; a scope
/// index is additionally bounded by the function's scope table.
fn verify_immediate_operand(
    function: &Function,
    function_index: usize,
    instruction_pc: usize,
    operand_index: usize,
    operand: Operand,
    domain: ImmediateDomain,
) -> Result<(), BytecodeVerifyError> {
    let Operand::Imm32(value) = operand else {
        return Err(BytecodeVerifyError::OperandDecode {
            function_index,
            instruction_pc,
            operand_index,
        });
    };
    if !domain.admits(value) {
        return Err(BytecodeVerifyError::ImmediateOperand {
            function_index,
            instruction_pc,
            op: function.code[instruction_pc].op,
            operand_index,
            value,
            domain,
        });
    }
    if domain == ImmediateDomain::ScopeIndex && value as usize >= function.scopes.len() {
        return Err(BytecodeVerifyError::ContextScope {
            function_index,
            instruction_pc,
            scope: value,
            scope_count: function.scopes.len(),
        });
    }
    Ok(())
}

fn verify_instruction_semantics(
    module: &BytecodeModule,
    function: &Function,
    function_index: usize,
    instruction_pc: usize,
    mapped_context: Option<u16>,
) -> Result<(), BytecodeVerifyError> {
    let instruction = &function.code[instruction_pc];
    match instruction.op {
        Op::ReturnDerived if !has_derived_this_slot(function) => {
            Err(BytecodeVerifyError::DerivedConstructorReturn {
                function_index,
                instruction_pc,
                op: instruction.op,
            })
        }
        Op::Return | Op::ReturnValue | Op::ReturnUndefined if has_derived_this_slot(function) => {
            Err(BytecodeVerifyError::DerivedConstructorReturn {
                function_index,
                instruction_pc,
                op: instruction.op,
            })
        }
        Op::CollectArguments | Op::CallForwardArguments => {
            let Some(expected) = mapped_context else {
                return Ok(());
            };
            let context_operand = if instruction.op == Op::CollectArguments {
                1
            } else {
                4
            };
            match function.code.operand(instruction, context_operand) {
                Some(Operand::Register(register)) if register == expected => Ok(()),
                Some(Operand::Register(register)) => {
                    Err(BytecodeVerifyError::MappedArgumentContext {
                        function_index,
                        instruction_pc: Some(instruction_pc),
                        register,
                        expected,
                    })
                }
                _ => Err(BytecodeVerifyError::OperandDecode {
                    function_index,
                    instruction_pc,
                    operand_index: context_operand,
                }),
            }
        }
        Op::GetTemplateObject => {
            let template_index = const_index_operand(function, function_index, instruction_pc, 1)?;
            if (template_index as usize) >= module.template_sites.len() {
                return Err(BytecodeVerifyError::TemplateIndex {
                    function_index,
                    instruction_pc,
                    template_index,
                    template_count: module.template_sites.len(),
                });
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn const_index_operand(
    function: &Function,
    function_index: usize,
    instruction_pc: usize,
    operand_index: usize,
) -> Result<u32, BytecodeVerifyError> {
    match function
        .code
        .operand(&function.code[instruction_pc], operand_index)
    {
        Some(Operand::ConstIndex(value)) => Ok(value),
        _ => Err(BytecodeVerifyError::OperandDecode {
            function_index,
            instruction_pc,
            operand_index,
        }),
    }
}

const fn expected_constant_kind(op: Op) -> BytecodeConstantKind {
    match op {
        Op::LoadNumber => BytecodeConstantKind::Number,
        Op::LoadBigInt => BytecodeConstantKind::BigInt,
        Op::LoadRegExp => BytecodeConstantKind::RegExp,
        Op::MakeFunction | Op::MakeClosure => BytecodeConstantKind::FunctionId,
        _ => BytecodeConstantKind::String,
    }
}

fn verify_function_id(function_id: u32, base: u32, end: u32) -> Result<(), ()> {
    if function_id < base || function_id >= end {
        return Err(());
    }
    Ok(())
}

fn verify_metadata_pc(
    function_index: usize,
    metadata: &'static str,
    pc: u32,
    instruction_count: usize,
) -> Result<(), BytecodeVerifyError> {
    if (pc as usize) >= instruction_count {
        return Err(BytecodeVerifyError::MetadataPc {
            function_index,
            metadata,
            pc,
            instruction_count,
        });
    }
    Ok(())
}

fn verify_source_span(
    function_index: usize,
    metadata: &'static str,
    (start, end): (u32, u32),
) -> Result<(), BytecodeVerifyError> {
    if end < start {
        return Err(BytecodeVerifyError::InvalidSourceSpan {
            function_index,
            metadata,
            start,
            end,
        });
    }
    Ok(())
}

/// Whether `function` keeps a derived constructor's `this` in a context slot.
fn has_derived_this_slot(function: &Function) -> bool {
    function.is_derived_constructor
        && function.scopes.iter().any(|scope| {
            scope
                .slots
                .iter()
                .any(|slot| slot.kind == crate::SlotKind::DerivedThis)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BindingStoreFallback, ClassHintSite, FunctionCodeBuilder, LookupGlobalMode,
        LookupRefTarget, MappedArgumentBinding, ModuleInit, ScopeDescriptor, ScopeFlags, ScopeKind,
        SlotDescriptor, SlotKind, SourceKind, SpanEntry, StoreRefMode, TemplateSite,
    };

    fn module_with(code: crate::FunctionCode) -> BytecodeModule {
        BytecodeModule {
            module: "<verify>".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                locals: 2,
                code,
                ..Function::default()
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        }
    }

    fn returning_module() -> BytecodeModule {
        let mut code = FunctionCodeBuilder::new();
        code.push(Op::ReturnUndefined, &[]);
        module_with(code.finish())
    }

    #[test]
    fn verified_carrier_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VerifiedBytecodeModule>();
    }

    #[test]
    fn arguments_reads_require_activation_metadata() {
        for op in [Op::LoadArgumentsLength, Op::LoadArgumentsElement] {
            let mut code = FunctionCodeBuilder::new();
            let operands = [Operand::Register(0), Operand::Register(1)];
            code.push(
                op,
                &operands[..if op == Op::LoadArgumentsLength { 1 } else { 2 }],
            );
            code.push(Op::ReturnUndefined, &[]);
            let mut module = module_with(code.finish());
            assert!(matches!(
                verify_module(&module),
                Err(BytecodeVerifyError::ArgumentsReadMetadata { .. })
            ));
            module.functions[0].needs_arguments = true;
            verify_module(&module).unwrap();
            module.functions[0].param_count = 1;
            assert!(matches!(
                verify_module(&module),
                Err(BytecodeVerifyError::ArgumentsReadMetadata { .. })
            ));
        }
    }

    #[test]
    fn valid_module_is_accepted_at_base_zero_and_rebased() {
        let module = returning_module();
        verify_module(&module).expect("base-zero module");
        let mut rebased = module;
        rebased.functions[0].id = 7;
        verify_module_at_base(&rebased, 7).expect("rebased module");
    }

    #[test]
    fn carrier_retains_execution_proofs_and_rebases_function_records() {
        let mut module = returning_module();
        module.functions[0].locals = 2;
        module.functions[0].scratch = 3;
        module.functions[0].param_count = 1;
        module.functions[0].class_hint_sites.push(ClassHintSite {
            pc: 0,
            class_function_id: 1,
        });
        module.functions.push(Function {
            id: 1,
            name: "child".to_string(),
            code: returning_module().functions.remove(0).code,
            ..Function::default()
        });
        module.constants.push(Constant::FunctionId { index: 1 });
        module.module_inits.push(ModuleInit {
            url: "test:child".to_string(),
            function_id: 1,
        });

        let verified = VerifiedBytecodeModule::new(module).expect("valid carrier");
        assert_eq!(verified.function_base(), 0);
        assert_eq!(verified.function(0).unwrap().register_count(), 6);
        let main_layout = verified.function(0).unwrap().layout().clone();

        let rebased = verified.rebase_to(7).expect("proof-preserving rebase");
        assert_eq!(rebased.function_base(), 7);
        assert_eq!(rebased.function(0).unwrap().layout(), &main_layout);
        assert_eq!(rebased.module().functions[0].id, 7);
        assert_eq!(rebased.module().functions[1].id, 8);
        assert_eq!(
            rebased.module().functions[0].class_hint_sites[0].class_function_id,
            8
        );
        assert!(matches!(
            rebased.module().constants[0],
            Constant::FunctionId { index: 8 }
        ));
        assert_eq!(rebased.module().module_inits[0].function_id, 8);
    }

    #[test]
    fn carrier_rebase_range_overflow_is_typed() {
        let verified = VerifiedBytecodeModule::new(returning_module()).expect("valid carrier");
        assert_eq!(
            verified.rebase_to(u32::MAX).unwrap_err(),
            BytecodeRebaseError::FunctionRangeOverflow {
                base: u32::MAX,
                function_count: 1,
            }
        );
    }

    #[test]
    fn empty_and_sparse_function_tables_are_rejected() {
        let mut module = returning_module();
        module.functions.clear();
        assert_eq!(
            verify_module(&module),
            Err(BytecodeVerifyError::EmptyFunctionTable)
        );
        let mut module = returning_module();
        module.functions[0].id = 1;
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::FunctionId { .. })
        ));
    }

    #[test]
    fn register_and_constant_operands_are_bounded_and_typed() {
        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::LoadString,
            &[Operand::Register(2), Operand::ConstIndex(0)],
        );
        code.push(Op::ReturnUndefined, &[]);
        let mut module = module_with(code.finish());
        module.constants.push(Constant::Number { bits: 0 });
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::RegisterOperand { register: 2, .. })
        ));
        module.functions[0].locals = 3;
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::ConstantKind {
                expected: BytecodeConstantKind::String,
                actual: BytecodeConstantKind::Number,
                ..
            })
        ));
        module.constants.clear();
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::ConstantIndex { .. })
        ));
    }

    #[test]
    fn function_references_and_metadata_are_bounded() {
        let mut module = returning_module();
        module.constants.push(Constant::FunctionId { index: 1 });
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::FunctionConstant { .. })
        ));
        module.constants.clear();
        module.functions[0].class_hint_sites.push(ClassHintSite {
            pc: 0,
            class_function_id: 1,
        });
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::ClassHintFunction { .. })
        ));
        module.functions[0].class_hint_sites.clear();
        module.module_inits.push(ModuleInit {
            url: "test:module".to_string(),
            function_id: 1,
        });
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::ModuleInitFunction { .. })
        ));
    }

    #[test]
    fn metadata_pc_span_and_template_shape_are_rejected() {
        let mut module = returning_module();
        module.functions[0].spans.push(SpanEntry {
            pc: 1,
            span: (0, 1),
        });
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::MetadataPc { .. })
        ));
        module.functions[0].spans.clear();
        module.template_sites.push(TemplateSite {
            cooked: vec![Some("x".to_string())],
            raw: Vec::new(),
        });
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::TemplateArity { .. })
        ));
    }

    fn scope(kind: ScopeKind, slots: &[(&str, SlotKind)]) -> ScopeDescriptor {
        ScopeDescriptor {
            kind,
            flags: ScopeFlags::default(),
            slots: slots
                .iter()
                .map(|(name, kind)| SlotDescriptor {
                    name: (*name).to_string(),
                    kind: *kind,
                    exported: false,
                })
                .collect(),
        }
    }

    fn module_with_instruction(op: Op, operands: &[Operand]) -> BytecodeModule {
        let mut code = FunctionCodeBuilder::new();
        code.push(op, operands);
        code.push(Op::ReturnUndefined, &[]);
        let mut module = module_with(code.finish());
        module.constants.push(Constant::String {
            utf16: "x".encode_utf16().collect(),
        });
        module
    }

    #[test]
    fn create_context_scope_index_is_bounded_by_the_scope_table() {
        let operands = |scope| {
            [
                Operand::Register(0),
                Operand::Register(1),
                Operand::Imm32(scope),
            ]
        };
        let mut module = module_with_instruction(Op::CreateContext, &operands(0));
        assert_eq!(
            verify_module(&module),
            Err(BytecodeVerifyError::ContextScope {
                function_index: 0,
                instruction_pc: 0,
                scope: 0,
                scope_count: 0,
            })
        );
        module.functions[0].scopes = vec![scope(ScopeKind::Block, &[("x", SlotKind::Let)])];
        verify_module(&module).expect("scope 0 exists");

        let negative = module_with_instruction(Op::CreateContext, &operands(-1));
        assert!(matches!(
            verify_module(&negative),
            Err(BytecodeVerifyError::ImmediateOperand {
                op: Op::CreateContext,
                operand_index: 2,
                value: -1,
                domain: ImmediateDomain::ScopeIndex,
                ..
            })
        ));
    }

    #[test]
    fn context_coordinates_never_name_the_reserved_slot() {
        let coord = |slot| ContextCoord { depth: 2, slot }.to_imm32();
        for op in [
            Op::LoadContextSlot,
            Op::LoadContextSlotChecked,
            Op::StoreContextSlot,
            Op::StoreContextSlotChecked,
            Op::BindThisContextSlot,
        ] {
            let valid = module_with_instruction(
                op,
                &[
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Imm32(coord(ContextCoord::MAX_SLOT)),
                ],
            );
            verify_module(&valid).unwrap_or_else(|error| panic!("{op:?}: {error}"));
            let reserved = (2 << 16) | i32::from(ContextCoord::RESERVED_SLOT);
            let invalid = module_with_instruction(
                op,
                &[
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Imm32(reserved),
                ],
            );
            assert!(
                matches!(
                    verify_module(&invalid),
                    Err(BytecodeVerifyError::ImmediateOperand {
                        operand_index: 2,
                        domain: ImmediateDomain::ContextCoord,
                        ..
                    })
                ),
                "{op:?} admitted the reserved slot"
            );
        }
    }

    #[test]
    fn lookup_immediates_are_admitted_only_in_their_domains() {
        let lookup = |op, imm| {
            module_with_instruction(
                op,
                &[
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::Imm32(imm),
                ],
            )
        };
        let rejects = |module: &BytecodeModule, domain| {
            matches!(
                verify_module(module),
                Err(BytecodeVerifyError::ImmediateOperand { domain: actual, .. }) if actual == domain
            )
        };
        for op in [
            Op::DeleteLookupSlot,
            Op::LoadLookupGlobal,
            Op::TypeofLookupGlobal,
            Op::DeleteLookupGlobal,
            Op::StoreVarScope,
        ] {
            verify_module(&lookup(op, i32::from(u16::MAX))).expect("maximal depth");
            assert!(rejects(&lookup(op, -1), ImmediateDomain::ContextDepth));
            assert!(rejects(&lookup(op, 1 << 16), ImmediateDomain::ContextDepth));
        }
        let strict_global = LookupGlobalMode {
            depth: 3,
            strict: true,
        };
        verify_module(&lookup(Op::StoreLookupGlobal, strict_global.to_imm32()))
            .expect("strict global mode");
        assert!(rejects(
            &lookup(Op::StoreLookupGlobal, 1 << 20),
            ImmediateDomain::LookupGlobalMode
        ));
        verify_module(&lookup(
            Op::ResolveLookupRef,
            LookupRefTarget::Global { depth: 1 }.to_imm32(),
        ))
        .expect("every ref target decodes");
        verify_module(&lookup(
            Op::StoreRef,
            StoreRefMode {
                slot: None,
                fallback: BindingStoreFallback::Mutable,
                strict: false,
            }
            .to_imm32(),
        ))
        .expect("global store-ref mode");
        assert!(rejects(
            &lookup(Op::StoreRef, 3 << 16),
            ImmediateDomain::StoreRefMode
        ));
        assert!(rejects(
            &lookup(Op::LoadLookupSlot, i32::from(ContextCoord::RESERVED_SLOT)),
            ImmediateDomain::ContextCoord
        ));

        let store_lookup_slot = |fallback| {
            module_with_instruction(
                Op::StoreLookupSlot,
                &[
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::Imm32(ContextCoord { depth: 1, slot: 0 }.to_imm32()),
                    Operand::Imm32(fallback),
                ],
            )
        };
        verify_module(&store_lookup_slot(
            BindingStoreFallback::ImmutableIgnore.to_imm32(),
        ))
        .expect("known fallback");
        assert!(rejects(
            &store_lookup_slot(3),
            ImmediateDomain::StoreFallback
        ));

        let declare = |depth| {
            module_with_instruction(
                Op::DeclareEvalVar,
                &[
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::Imm32(depth),
                ],
            )
        };
        verify_module(&declare(0)).expect("var scope at hop 0");
        assert!(rejects(&declare(-5), ImmediateDomain::ContextDepth));
    }

    #[test]
    fn scope_descriptors_are_well_formed() {
        let mut module = returning_module();
        module.functions[0].scopes = vec![
            scope(
                ScopeKind::Params,
                &[("a", SlotKind::Param { checked: false })],
            ),
            scope(
                ScopeKind::Body,
                &[("x", SlotKind::Var), ("x", SlotKind::FunctionDecl)],
            ),
        ];
        assert_eq!(
            verify_module(&module),
            Err(BytecodeVerifyError::ScopeDescriptor {
                function_index: 0,
                scope_index: 1,
                defect: ScopeDescriptorDefect::DuplicateSlotName { slot_index: 1 },
            })
        );

        module.functions[0].scopes[1] = scope(ScopeKind::Body, &[("x", SlotKind::Var)]);
        module.functions[0].scopes[1].flags = ScopeFlags {
            strict: false,
            var_scope: true,
            has_extension: true,
        };
        verify_module(&module).expect("sloppy extension anchor");
        module.functions[0].scopes[1].flags.strict = true;
        assert_eq!(
            verify_module(&module),
            Err(BytecodeVerifyError::ScopeDescriptor {
                function_index: 0,
                scope_index: 1,
                defect: ScopeDescriptorDefect::StrictExtension,
            })
        );

        module.functions[0].scopes = vec![ScopeDescriptor {
            kind: ScopeKind::Block,
            flags: ScopeFlags::default(),
            slots: (0..=usize::from(ContextCoord::MAX_SLOT) + 1)
                .map(|index| SlotDescriptor {
                    name: format!("s{index}"),
                    kind: SlotKind::Synthetic,
                    exported: false,
                })
                .collect(),
        }];
        assert_eq!(
            verify_module(&module),
            Err(BytecodeVerifyError::ScopeDescriptor {
                function_index: 0,
                scope_index: 0,
                defect: ScopeDescriptorDefect::TooManySlots {
                    slot_count: usize::from(ContextCoord::MAX_SLOT) + 2,
                },
            })
        );
        module.functions[0].scopes[0].slots.pop();
        verify_module(&module).expect("a full context is addressable");

        module.functions[0].scopes = vec![scope(ScopeKind::Block, &[]); usize::from(u16::MAX) + 2];
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::ScopeDescriptor {
                defect: ScopeDescriptorDefect::TooManyScopes { .. },
                ..
            })
        ));
    }

    #[test]
    fn mapped_formals_and_forwarded_arguments_share_one_context_register() {
        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::CallForwardArguments,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(2),
            ],
        );
        code.push(Op::ReturnUndefined, &[]);
        let mut module = module_with(code.finish());
        module.functions[0].param_count = 2;
        module.functions[0].locals = 2;
        let mapped = |argument_index, reg, slot| MappedArgumentBinding {
            argument_index,
            formal_name: format!("p{argument_index}"),
            storage: ArgumentBindingStorage::Context { reg, slot },
        };
        module.functions[0].mapped_argument_bindings = vec![mapped(0, 2, 0), mapped(1, 2, 1)];
        verify_module(&module).expect("forward names the parameter context");

        module.functions[0].mapped_argument_bindings[1] = mapped(1, 3, 1);
        assert_eq!(
            verify_module(&module),
            Err(BytecodeVerifyError::MappedArgumentContext {
                function_index: 0,
                instruction_pc: None,
                register: 3,
                expected: 2,
            })
        );

        module.functions[0].mapped_argument_bindings = vec![mapped(0, 3, 0)];
        assert_eq!(
            verify_module(&module),
            Err(BytecodeVerifyError::MappedArgumentContext {
                function_index: 0,
                instruction_pc: Some(0),
                register: 2,
                expected: 3,
            })
        );

        module.functions[0].mapped_argument_bindings = vec![mapped(0, 9, 0)];
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::MappedArgument {
                field: "context register",
                index: 9,
                ..
            })
        ));
        module.functions[0].mapped_argument_bindings =
            vec![mapped(0, 2, ContextCoord::RESERVED_SLOT)];
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::MappedArgument {
                field: "context slot",
                ..
            })
        ));
    }

    #[test]
    fn make_closure_names_its_context_register() {
        let mut module = returning_module();
        module.functions.push(Function {
            id: 1,
            name: "child".to_string(),
            code: returning_module().functions.remove(0).code,
            ..Function::default()
        });
        module.constants.push(Constant::FunctionId { index: 1 });
        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::MakeClosure,
            &[
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(1),
            ],
        );
        code.push(Op::ReturnUndefined, &[]);
        module.functions[0].code = code.finish();
        verify_module(&module).expect("closure over r1");

        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::MakeClosure,
            &[
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(7),
            ],
        );
        code.push(Op::ReturnUndefined, &[]);
        module.functions[0].code = code.finish();
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::RegisterOperand {
                op: Op::MakeClosure,
                operand_index: 2,
                register: 7,
                ..
            })
        ));
    }

    #[test]
    fn derived_constructors_complete_only_through_return_derived() {
        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::ReturnDerived,
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::Imm32(ContextCoord::new(0, 0).expect("coord").to_imm32()),
            ],
        );
        let mut module = module_with(code.finish());
        assert_eq!(
            verify_module(&module),
            Err(BytecodeVerifyError::DerivedConstructorReturn {
                function_index: 0,
                instruction_pc: 0,
                op: Op::ReturnDerived,
            })
        );
        module.functions[0].is_derived_constructor = true;
        let derived_this = scope(ScopeKind::Params, &[("this", SlotKind::DerivedThis)]);
        module.functions[0].scopes = vec![derived_this.clone()];
        verify_module(&module).expect("derived constructor return");

        // A frame-held `this` keeps the ordinary return family.
        let mut frame_this = returning_module();
        frame_this.functions[0].is_derived_constructor = true;
        verify_module(&frame_this).expect("frame-held derived this returns plainly");

        let mut plain = returning_module();
        plain.functions[0].is_derived_constructor = true;
        plain.functions[0].scopes = vec![derived_this];
        assert_eq!(
            verify_module(&plain),
            Err(BytecodeVerifyError::DerivedConstructorReturn {
                function_index: 0,
                instruction_pc: 0,
                op: Op::ReturnUndefined,
            })
        );
    }

    #[test]
    fn immediate_right_operators_reject_an_aliased_destination() {
        // The lowering that gives these forms their register economy writes
        // the constant into the destination first, so an aliased destination
        // would destroy the left operand. Generated code diverges from the
        // interpreter there, which is why admission has to reject it.
        for op in [
            Op::AddImm,
            Op::SubImm,
            Op::BitwiseAndImm,
            Op::LessThanImm,
            Op::EqualImm,
            Op::NotEqualImm,
        ] {
            let mut aliased_code = FunctionCodeBuilder::new();
            aliased_code.push(
                op,
                &[
                    Operand::Register(0),
                    Operand::Register(0),
                    Operand::Imm32(1),
                ],
            );
            aliased_code.push(Op::Return, &[Operand::Register(0)]);
            let aliased = module_with(aliased_code.finish());
            assert!(
                matches!(
                    verify_module(&aliased),
                    Err(BytecodeVerifyError::ImmediateOperandAliasesDestination {
                        register: 0,
                        ..
                    })
                ),
                "{op:?} with an aliased destination was accepted"
            );

            let mut distinct_code = FunctionCodeBuilder::new();
            distinct_code.push(
                op,
                &[
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::Imm32(1),
                ],
            );
            distinct_code.push(Op::Return, &[Operand::Register(1)]);
            let distinct = module_with(distinct_code.finish());
            verify_module(&distinct)
                .unwrap_or_else(|error| panic!("{op:?} with a distinct destination: {error}"));
        }
    }

    #[test]
    fn template_object_index_is_bounded() {
        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::GetTemplateObject,
            &[Operand::Register(0), Operand::ConstIndex(0)],
        );
        code.push(Op::ReturnUndefined, &[]);
        let module = module_with(code.finish());
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::TemplateIndex {
                template_index: 0,
                template_count: 0,
                ..
            })
        ));
    }
}
