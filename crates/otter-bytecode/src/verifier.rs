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
//! - Register, constant, function, upvalue, and metadata indices are bounded by
//!   their owning tables.
//! - A [`VerifiedBytecodeModule`] exposes no mutable access to its module, and
//!   proof-preserving rebasing changes only function-id-bearing records.
//! - Successful verification does not publish runtime state.
//!
//! # See also
//! - [`crate::encoding::layout_wordcode_function`]
//! - [`crate::binary`]

use crate::encoding::{FunctionLayout, VerifyError, layout_wordcode_function};
use crate::opcode_schema::{
    BindingDelete, BindingRead, BindingSemantics, BindingWrite, RegisterAccess, opcode_schema,
    register_access_at,
};
use crate::{ArgumentBindingStorage, BytecodeModule, Constant, Function, Op, Operand};

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
    /// An instruction addresses an upvalue outside the owning frame spine.
    UpvalueOperand {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Opcode at the site.
        op: Op,
        /// Operand position carrying the upvalue index.
        operand_index: usize,
        /// Signed decoded upvalue index.
        upvalue: i32,
        /// Exclusive bound for this operation (`own` or `own + inherited`).
        upvalue_count: u32,
        /// Upvalue domain selected by the opcode.
        domain: &'static str,
    },
    /// `MakeFunction` cannot instantiate a function that requires captures.
    FunctionRequiresCaptures {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Referenced target function id.
        target_function_id: u32,
        /// Captures required by the target.
        inherited_upvalue_count: u16,
    },
    /// `MakeClosure` capture arity differs from the target function's spine.
    ClosureCaptureCount {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Referenced target function id.
        target_function_id: u32,
        /// Captures required by the target.
        expected: u16,
        /// Captures encoded at this site.
        actual: u32,
    },
    /// One `MakeClosure` parent-capture index is outside the current frame.
    ClosureCaptureOperand {
        /// Owning function table index.
        function_index: usize,
        /// Logical instruction PC.
        instruction_pc: usize,
        /// Zero-based capture position.
        capture_index: usize,
        /// Signed parent-upvalue index.
        parent_upvalue: i32,
        /// Exact current-frame upvalue spine size.
        upvalue_count: u32,
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
    /// Mapped-arguments metadata points outside parameters/registers/upvalues.
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
    /// Direct-eval metadata points outside the declared upvalue spine.
    DirectEvalUpvalue {
        /// Owning function table index.
        function_index: usize,
        /// Binding vector index.
        binding_index: usize,
        /// Encoded upvalue index.
        upvalue: u16,
        /// Exact upvalue-spine size.
        upvalue_count: u32,
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
            Self::UpvalueOperand {
                function_index,
                instruction_pc,
                op,
                operand_index,
                upvalue,
                upvalue_count,
                domain,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} {op:?} operand {operand_index} {domain} upvalue {upvalue} is outside 0..{upvalue_count}"
            ),
            Self::FunctionRequiresCaptures {
                function_index,
                instruction_pc,
                target_function_id,
                inherited_upvalue_count,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} MakeFunction target {target_function_id} requires {inherited_upvalue_count} captures"
            ),
            Self::ClosureCaptureCount {
                function_index,
                instruction_pc,
                target_function_id,
                expected,
                actual,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} MakeClosure target {target_function_id} requires {expected} captures, encoded {actual}"
            ),
            Self::ClosureCaptureOperand {
                function_index,
                instruction_pc,
                capture_index,
                parent_upvalue,
                upvalue_count,
            } => write!(
                f,
                "function {function_index} instruction {instruction_pc} MakeClosure capture {capture_index} parent upvalue {parent_upvalue} is outside 0..{upvalue_count}"
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
            Self::DirectEvalUpvalue {
                function_index,
                binding_index,
                upvalue,
                upvalue_count,
            } => write!(
                f,
                "function {function_index} direct-eval binding {binding_index} upvalue {upvalue} is outside 0..{upvalue_count}"
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

    let upvalue_count = u32::from(function.own_upvalue_count)
        .checked_add(u32::from(function.inherited_upvalue_count))
        .expect("two u16 counts fit u32");
    for (binding_index, binding) in function.mapped_argument_bindings.iter().enumerate() {
        if u32::from(binding.argument_index) >= u32::from(function.param_count) {
            return Err(BytecodeVerifyError::MappedArgument {
                function_index,
                binding_index,
                field: "argument",
                index: u32::from(binding.argument_index),
                limit: u32::from(function.param_count),
            });
        }
        let (field, index, limit) = match binding.storage {
            ArgumentBindingStorage::Register { reg } => {
                ("register", u32::from(reg), register_count)
            }
            ArgumentBindingStorage::Upvalue { idx } => (
                "upvalue",
                u32::from(idx),
                u32::from(function.own_upvalue_count),
            ),
        };
        if index >= limit {
            return Err(BytecodeVerifyError::MappedArgument {
                function_index,
                binding_index,
                field,
                index,
                limit,
            });
        }
    }
    for (binding_index, binding) in function.direct_eval_bindings.iter().enumerate() {
        if u32::from(binding.upvalue) >= upvalue_count {
            return Err(BytecodeVerifyError::DirectEvalUpvalue {
                function_index,
                binding_index,
                upvalue: binding.upvalue,
                upvalue_count,
            });
        }
    }

    for (instruction_pc, instruction) in function.code.iter().enumerate() {
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
            function_base,
            function_end,
            upvalue_count,
        )?;
    }
    Ok(VerifiedFunction {
        layout,
        register_count: register_count as u16,
    })
}

#[allow(clippy::too_many_arguments)]
fn verify_instruction_semantics(
    module: &BytecodeModule,
    function: &Function,
    function_index: usize,
    instruction_pc: usize,
    function_base: u32,
    function_end: u32,
    upvalue_count: u32,
) -> Result<(), BytecodeVerifyError> {
    let instruction = &function.code[instruction_pc];
    if let Some(binding) = opcode_schema(instruction.op).binding {
        let upvalue_operand = match binding {
            BindingSemantics::Read(BindingRead::Upvalue { index, .. })
            | BindingSemantics::Read(BindingRead::ShadowedUpvalue { index, .. })
            | BindingSemantics::Write(BindingWrite::Upvalue { index, .. })
            | BindingSemantics::Write(BindingWrite::ShadowedUpvalue { index, .. })
            | BindingSemantics::Write(BindingWrite::ShadowedRestore { index, .. })
            | BindingSemantics::Delete(BindingDelete::ShadowedUpvalue { index, .. }) => {
                Some(usize::from(index))
            }
            _ => None,
        };
        if let Some(operand_index) = upvalue_operand {
            verify_upvalue_operand(
                function,
                function_index,
                instruction_pc,
                operand_index,
                upvalue_count,
                "frame",
            )?;
        }
    }

    match instruction.op {
        Op::FreshUpvalue => verify_upvalue_operand(
            function,
            function_index,
            instruction_pc,
            0,
            u32::from(function.own_upvalue_count),
            "own",
        ),
        Op::MakeFunction => {
            let (target_function_id, target) = instruction_function_target(
                module,
                function,
                function_index,
                instruction_pc,
                1,
                function_base,
                function_end,
            )?;
            if target.inherited_upvalue_count != 0 {
                return Err(BytecodeVerifyError::FunctionRequiresCaptures {
                    function_index,
                    instruction_pc,
                    target_function_id,
                    inherited_upvalue_count: target.inherited_upvalue_count,
                });
            }
            Ok(())
        }
        Op::MakeClosure => verify_make_closure(
            module,
            function,
            function_index,
            instruction_pc,
            function_base,
            function_end,
            upvalue_count,
        ),
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

fn verify_upvalue_operand(
    function: &Function,
    function_index: usize,
    instruction_pc: usize,
    operand_index: usize,
    upvalue_count: u32,
    domain: &'static str,
) -> Result<(), BytecodeVerifyError> {
    let instruction = &function.code[instruction_pc];
    let upvalue = imm32_operand(function, function_index, instruction_pc, operand_index)?;
    if upvalue < 0 || (upvalue as u32) >= upvalue_count {
        return Err(BytecodeVerifyError::UpvalueOperand {
            function_index,
            instruction_pc,
            op: instruction.op,
            operand_index,
            upvalue,
            upvalue_count,
            domain,
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_make_closure(
    module: &BytecodeModule,
    function: &Function,
    function_index: usize,
    instruction_pc: usize,
    function_base: u32,
    function_end: u32,
    upvalue_count: u32,
) -> Result<(), BytecodeVerifyError> {
    let (target_function_id, target) = instruction_function_target(
        module,
        function,
        function_index,
        instruction_pc,
        1,
        function_base,
        function_end,
    )?;
    let actual = const_index_operand(function, function_index, instruction_pc, 2)?;
    if actual != u32::from(target.inherited_upvalue_count) {
        return Err(BytecodeVerifyError::ClosureCaptureCount {
            function_index,
            instruction_pc,
            target_function_id,
            expected: target.inherited_upvalue_count,
            actual,
        });
    }
    for capture_index in 0..actual as usize {
        let parent_upvalue =
            imm32_operand(function, function_index, instruction_pc, 3 + capture_index)?;
        if parent_upvalue < 0 || (parent_upvalue as u32) >= upvalue_count {
            return Err(BytecodeVerifyError::ClosureCaptureOperand {
                function_index,
                instruction_pc,
                capture_index,
                parent_upvalue,
                upvalue_count,
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn instruction_function_target<'a>(
    module: &'a BytecodeModule,
    function: &Function,
    function_index: usize,
    instruction_pc: usize,
    operand_index: usize,
    function_base: u32,
    function_end: u32,
) -> Result<(u32, &'a Function), BytecodeVerifyError> {
    let constant_index =
        const_index_operand(function, function_index, instruction_pc, operand_index)?;
    let Some(Constant::FunctionId {
        index: target_function_id,
    }) = module.constants.get(constant_index as usize)
    else {
        return Err(BytecodeVerifyError::ConstantIndex {
            function_index,
            instruction_pc,
            op: function.code[instruction_pc].op,
            operand_index,
            constant_index,
            constant_count: module.constants.len(),
        });
    };
    let local_index = target_function_id
        .checked_sub(function_base)
        .and_then(|index| usize::try_from(index).ok())
        .filter(|index| *index < module.functions.len())
        .ok_or(BytecodeVerifyError::FunctionConstant {
            constant_index: constant_index as usize,
            function_id: *target_function_id,
            function_base,
            function_end,
        })?;
    Ok((*target_function_id, &module.functions[local_index]))
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

fn imm32_operand(
    function: &Function,
    function_index: usize,
    instruction_pc: usize,
    operand_index: usize,
) -> Result<i32, BytecodeVerifyError> {
    match function
        .code
        .operand(&function.code[instruction_pc], operand_index)
    {
        Some(Operand::Imm32(value)) => Ok(value),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClassHintSite, FunctionCodeBuilder, ModuleInit, SourceKind, SpanEntry, TemplateSite,
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

    #[test]
    fn instruction_upvalue_domains_are_enforced() {
        let mut code = FunctionCodeBuilder::new();
        code.push(Op::LoadUpvalue, &[Operand::Register(0), Operand::Imm32(1)]);
        code.push(Op::ReturnUndefined, &[]);
        let mut module = module_with(code.finish());
        module.functions[0].own_upvalue_count = 1;
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::UpvalueOperand {
                op: Op::LoadUpvalue,
                upvalue: 1,
                upvalue_count: 1,
                domain: "frame",
                ..
            })
        ));

        let mut code = FunctionCodeBuilder::new();
        code.push(Op::FreshUpvalue, &[Operand::Imm32(0)]);
        code.push(Op::ReturnUndefined, &[]);
        module.functions[0].code = code.finish();
        module.functions[0].own_upvalue_count = 0;
        module.functions[0].inherited_upvalue_count = 1;
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::UpvalueOperand {
                op: Op::FreshUpvalue,
                upvalue: 0,
                upvalue_count: 0,
                domain: "own",
                ..
            })
        ));
    }

    #[test]
    fn closure_capture_shape_matches_target_and_parent_spines() {
        let child_code = {
            let mut code = FunctionCodeBuilder::new();
            code.push(Op::ReturnUndefined, &[]);
            code.finish()
        };
        let mut module = returning_module();
        module.functions[0].locals = 1;
        module.functions[0].own_upvalue_count = 1;
        module.functions.push(Function {
            id: 1,
            name: "child".to_string(),
            inherited_upvalue_count: 1,
            code: child_code,
            ..Function::default()
        });
        module.constants.push(Constant::FunctionId { index: 1 });

        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::MakeClosure,
            &[
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(0),
            ],
        );
        code.push(Op::ReturnUndefined, &[]);
        module.functions[0].code = code.finish();
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::ClosureCaptureCount {
                target_function_id: 1,
                expected: 1,
                actual: 0,
                ..
            })
        ));

        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::MakeClosure,
            &[
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(1),
                Operand::Imm32(1),
            ],
        );
        code.push(Op::ReturnUndefined, &[]);
        module.functions[0].code = code.finish();
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::ClosureCaptureOperand {
                capture_index: 0,
                parent_upvalue: 1,
                upvalue_count: 1,
                ..
            })
        ));

        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::MakeClosure,
            &[
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(1),
                Operand::Imm32(0),
            ],
        );
        code.push(Op::ReturnUndefined, &[]);
        module.functions[0].code = code.finish();
        verify_module(&module).expect("matching closure capture spine");

        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::MakeFunction,
            &[Operand::Register(0), Operand::ConstIndex(0)],
        );
        code.push(Op::ReturnUndefined, &[]);
        module.functions[0].code = code.finish();
        assert!(matches!(
            verify_module(&module),
            Err(BytecodeVerifyError::FunctionRequiresCaptures {
                target_function_id: 1,
                inherited_upvalue_count: 1,
                ..
            })
        ));
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
