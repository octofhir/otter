//! Frozen execution bytecode for the VM dispatch loop.
//!
//! `otter-bytecode` owns the compiler/debug DTO shape. The VM owns this
//! compact view so hot dispatch reads opcodes, operands, byte offsets,
//! and named-property IC sites directly off each instruction record.
//!
//! # Contents
//! - [`ExecutableModuleBuilder`] — transient builder over compiler bytecode.
//! - [`ExecutableModule`] — VM-owned frozen function table.
//! - [`ExecutableFunction`] — one function body: instruction stream,
//!   byte-stream length, byte-offset source-map spans.
//! - [`ExecInstr`] — single instruction record: opcode, owned operands,
//!   byte length, byte-offset PC, optional IC site id.
//!
//! # Invariants
//! - `frame.pc` is a byte offset into the function's encoded stream.
//! - Each `ExecInstr` carries its own `byte_pc` and `byte_len` so the
//!   dispatch loop advances by `byte_len` and resolves jump targets in the
//!   same coordinate system as the source-map spans.
//! - Operands live in a per-instruction `Box<[Operand]>`; there is no
//!   shared side table. Variadic opcodes just hold a longer slice.
//! - Branch-class `Imm32` operands hold byte-offset deltas relative to
//!   `(byte_pc + 1)`. `NO_HANDLER_OFFSET` is preserved verbatim for absent
//!   try-handler slots.
//! - Named property IC sites receive dense VM-local ids during build; the
//!   bytecode JSON dump stays unchanged.
//!
//! # See also
//! - [`crate::execution_context`]
//! - [`otter_bytecode::Instruction`]

use otter_bytecode::{
    ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, Function, NO_HANDLER_OFFSET, Op,
    Operand, SpanEntry,
    encoding::{EncodedFunction, encode_function, translate_spans_to_byte_pcs},
};

const NO_PROPERTY_IC_SITE: u32 = u32::MAX;

/// Transient builder for [`ExecutableModule`].
///
/// The builder owns dense IC-site assignment while the VM creates an
/// [`crate::ExecutionContext`]. Dispatch receives only the frozen
/// [`ExecutableModule`] produced by [`Self::freeze`].
#[derive(Debug, Default)]
pub(crate) struct ExecutableModuleBuilder {
    functions: Vec<ExecutableFunction>,
    next_property_ic_site: u32,
}

impl ExecutableModuleBuilder {
    /// Build a transient executable view from the compiler/debug module DTO.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_bytecode(module: &BytecodeModule) -> Self {
        Self::from_bytecode_with_ic_base(module, 0)
    }

    /// Build a transient executable view whose dense property-IC site
    /// ids start at `property_ic_base`, keeping sites globally unique
    /// across chunks linked into one interpreter.
    #[must_use]
    pub(crate) fn from_bytecode_with_ic_base(
        module: &BytecodeModule,
        property_ic_base: u32,
    ) -> Self {
        let mut builder = Self {
            functions: Vec::with_capacity(module.functions.len()),
            next_property_ic_site: property_ic_base,
        };
        for function in &module.functions {
            builder.push_function(function);
        }
        builder
    }

    fn push_function(&mut self, function: &Function) {
        let function = ExecutableFunction::from_bytecode(function, &mut self.next_property_ic_site);
        self.functions.push(function);
    }

    /// Seal mutable build buffers into the VM-owned frozen execution product.
    #[must_use]
    pub(crate) fn freeze(self) -> ExecutableModule {
        ExecutableModule {
            functions: self.functions.into_boxed_slice(),
            property_ic_site_end: self.next_property_ic_site,
        }
    }
}

/// VM-owned executable view of a bytecode module.
#[derive(Debug, Clone)]
pub(crate) struct ExecutableModule {
    functions: Box<[ExecutableFunction]>,
    property_ic_site_end: u32,
}

impl ExecutableModule {
    /// Build a frozen execution view from the compiler/debug module DTO.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_bytecode(module: &BytecodeModule) -> Self {
        ExecutableModuleBuilder::from_bytecode(module).freeze()
    }

    /// Build a frozen execution view whose dense property-IC site ids
    /// start at `property_ic_base`.
    #[must_use]
    pub(crate) fn from_bytecode_with_ic_base(
        module: &BytecodeModule,
        property_ic_base: u32,
    ) -> Self {
        ExecutableModuleBuilder::from_bytecode_with_ic_base(module, property_ic_base).freeze()
    }

    /// Function-table lookup by chunk-local function index.
    #[must_use]
    pub(crate) fn function(&self, local_index: u32) -> Option<&ExecutableFunction> {
        self.functions.get(local_index as usize)
    }

    /// Return an instruction's operands in declaration order.
    #[must_use]
    pub(crate) fn operands<'a>(&self, instr: &'a ExecInstr) -> &'a [Operand] {
        instr.operands()
    }

    /// Return one instruction operand by index without materialising the
    /// whole operand slice at the call site.
    #[must_use]
    pub(crate) fn operand<'a>(&self, instr: &'a ExecInstr, index: usize) -> Option<&'a Operand> {
        instr.operand(index)
    }

    /// Decode one register operand.
    #[must_use]
    pub(crate) fn register(&self, instr: &ExecInstr, index: usize) -> Option<u16> {
        instr.register(index)
    }

    /// Decode one constant-pool index operand.
    #[must_use]
    pub(crate) fn const_index(&self, instr: &ExecInstr, index: usize) -> Option<u32> {
        instr.const_index(index)
    }

    /// Decode one signed immediate operand.
    #[must_use]
    pub(crate) fn imm32(&self, instr: &ExecInstr, index: usize) -> Option<i32> {
        instr.imm32(index)
    }

    /// One past the highest dense named-property IC site id in this
    /// module (equals the site count when the IC base is zero).
    #[must_use]
    pub(crate) const fn property_ic_site_end(&self) -> u32 {
        self.property_ic_site_end
    }
}

impl ExecutableFunction {
    /// Byte-offset source-map entries, sorted by `pc`. Empty when the
    /// underlying [`Function::spans`] is empty.
    #[must_use]
    pub(crate) fn byte_spans(&self) -> &[SpanEntry] {
        &self.byte_spans
    }

    /// Total length in bytes of this function's encoded stream.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn code_byte_len(&self) -> u32 {
        self.code_byte_len
    }

    /// Resolve a byte-offset PC to its `ExecInstr`. Returns `None` when
    /// `byte_pc` does not fall on an instruction boundary (which only
    /// happens on corrupt bytecode).
    #[must_use]
    pub(crate) fn instr_at_byte_pc(&self, byte_pc: u32) -> Option<&ExecInstr> {
        let idx = self
            .code
            .binary_search_by_key(&byte_pc, |instr| instr.byte_pc())
            .ok()?;
        self.code.get(idx)
    }
}

/// One executable function body.
#[derive(Debug, Clone)]
pub(crate) struct ExecutableFunction {
    /// Global VM function id (chunk base + local table index).
    pub(crate) id: u32,
    /// Number of parameter registers at the start of the frame.
    pub(crate) param_count: u16,
    /// Total register window size: params + locals + scratch.
    pub(crate) register_count: u16,
    /// Number of fresh upvalue cells owned by each frame.
    pub(crate) own_upvalue_count: u16,
    /// `true` when this function uses strict-mode call semantics.
    pub(crate) is_strict: bool,
    /// `true` when this function is an arrow function.
    pub(crate) is_arrow: bool,
    /// `true` when this function is a MethodDefinition body (class
    /// or object-literal method / accessor) — never a constructor,
    /// carries no implicit `prototype` property.
    pub(crate) is_method: bool,
    /// `true` when this function declares a rest parameter.
    pub(crate) has_rest: bool,
    /// `true` when this function is async.
    pub(crate) is_async: bool,
    /// `true` when this function is a generator.
    pub(crate) is_generator: bool,
    /// `true` when this function is an async generator.
    pub(crate) is_async_generator: bool,
    /// `true` when this function is a derived-class constructor whose
    /// `this` is bound by `super(...)` (§10.2.2). Frame setup starts
    /// it in the TDZ.
    pub(crate) is_derived_constructor: bool,
    /// `true` when this function body needs an `arguments` object.
    pub(crate) needs_arguments: bool,
    /// Arguments object shape requested by the compiler.
    pub(crate) arguments_object_kind: ArgumentsObjectKind,
    /// Compact mapped-arguments bindings without debug-only formal names.
    pub(crate) mapped_argument_bindings: Box<[ExecMappedArgumentBinding]>,
    /// `true` when this function is an ES module body.
    pub(crate) is_module: bool,
    /// Source module URL carried by frames for module resolution.
    pub(crate) module_url: Box<str>,
    /// §19.2.1.3 — name → own-upvalue table for direct eval. On a
    /// function containing a direct eval call site this lists every
    /// function-scope binding; on a compiled eval `<main>` it lists
    /// the new var-scoped bindings the body introduced.
    pub(crate) direct_eval_bindings: Box<[ExecDirectEvalBinding]>,
    /// §19.2.1.1 `inFunction` signal for `Op::Eval` — `true` when
    /// this function contains a direct eval call site (the binding
    /// table may still be empty).
    pub(crate) contains_direct_eval: bool,
    /// Hot instruction stream. Indexed in source order; the dispatch
    /// loop resolves a frame's byte-offset PC to an entry via
    /// [`Self::instr_at_byte_pc`] (`O(log N)` binary search on `byte_pc`).
    pub(crate) code: Box<[ExecInstr]>,
    /// Source-map entries with `pc` expressed as a byte offset into the
    /// encoded stream. Empty when the underlying [`Function::spans`] is empty.
    pub(crate) byte_spans: Box<[SpanEntry]>,
    /// Total length in bytes of this function's encoded stream. Acts as
    /// the upper bound for jump targets that fall off the end of the
    /// last instruction.
    pub(crate) code_byte_len: u32,
}

impl ExecutableFunction {
    fn from_bytecode(function: &Function, next_property_ic_site: &mut u32) -> Self {
        let register_count = function
            .param_count
            .saturating_add(function.locals)
            .saturating_add(function.scratch);
        let EncodedFunction {
            code: code_bytes,
            instr_to_byte_pc,
        } = encode_function(&function.code);
        let code_byte_len =
            u32::try_from(code_bytes.len()).expect("function byte stream exceeds u32 range");
        let code = function
            .code
            .iter()
            .enumerate()
            .map(|(idx, instr)| {
                let property_ic_site = match instr.op {
                    Op::LoadProperty | Op::StoreProperty | Op::HasProperty => {
                        let site = *next_property_ic_site;
                        *next_property_ic_site = next_property_ic_site
                            .checked_add(1)
                            .expect("property IC site table exceeds u32");
                        site
                    }
                    _ => NO_PROPERTY_IC_SITE,
                };
                let byte_pc = instr_to_byte_pc[idx];
                let next_byte_pc = instr_to_byte_pc
                    .get(idx + 1)
                    .copied()
                    .unwrap_or(code_byte_len);
                let byte_len = u16::try_from(next_byte_pc - byte_pc)
                    .expect("single instruction exceeds 65535-byte encoding");
                // Compiler emits branch deltas in instruction-index units;
                // the dispatcher resolves them as byte offsets relative to
                // `(byte_pc + 1)` (the byte right after the opcode), so the
                // executable builder rewrites each branch operand into the
                // dispatcher's coordinate system here.
                let operands = rewrite_branch_operands(
                    instr.op,
                    instr.operands.as_slice(),
                    idx,
                    byte_pc,
                    &instr_to_byte_pc,
                    code_byte_len,
                );
                ExecInstr::from_operands(instr.op, operands, property_ic_site, byte_pc, byte_len)
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let mapped_argument_bindings = function
            .mapped_argument_bindings
            .iter()
            .map(|binding| ExecMappedArgumentBinding {
                argument_index: binding.argument_index,
                storage: binding.storage,
            })
            .collect();
        let byte_spans =
            translate_spans_to_byte_pcs(&function.spans, &instr_to_byte_pc, code_byte_len)
                .into_boxed_slice();
        Self {
            id: function.id,
            param_count: function.param_count,
            register_count,
            own_upvalue_count: function.own_upvalue_count,
            is_strict: function.is_strict,
            is_arrow: function.is_arrow,
            is_method: function.is_method,
            has_rest: function.has_rest,
            is_derived_constructor: function.is_derived_constructor,
            is_async: function.is_async,
            is_generator: function.is_generator,
            is_async_generator: function.is_async_generator,
            needs_arguments: function.needs_arguments,
            arguments_object_kind: function.arguments_object_kind,
            mapped_argument_bindings,
            is_module: function.is_module,
            module_url: function.module_url.clone().into_boxed_str(),
            direct_eval_bindings: function
                .direct_eval_bindings
                .iter()
                .map(|binding| ExecDirectEvalBinding {
                    name: binding.name.clone().into_boxed_str(),
                    upvalue: binding.upvalue,
                    lexical: binding.lexical,
                })
                .collect(),
            contains_direct_eval: function.contains_direct_eval,
            code,
            byte_spans,
            code_byte_len,
        }
    }
}

/// Translate branch-class `Imm32` operands from compiler-emitted
/// instruction-index deltas into byte-offset deltas relative to
/// `(jump_byte_pc + 1)`. Non-branch opcodes pass through.
fn rewrite_branch_operands(
    op: Op,
    operands: &[Operand],
    jump_idx: usize,
    jump_byte_pc: u32,
    instr_to_byte_pc: &[u32],
    code_byte_len: u32,
) -> Vec<Operand> {
    let branch_slots: &[usize] = match op {
        Op::Jump | Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => &[0],
        // `JumpViaFinally` operand 0 is the branch delta; operand 1 is
        // the handler-stack floor (not a branch target).
        Op::JumpViaFinally => &[0],
        Op::EnterTry => &[0, 1],
        _ => return operands.to_vec(),
    };
    let mut out = operands.to_vec();
    for &slot in branch_slots {
        let Some(Operand::Imm32(raw)) = out.get(slot).copied() else {
            continue;
        };
        if raw == NO_HANDLER_OFFSET {
            continue;
        }
        let target_idx = jump_idx as i64 + 1 + raw as i64;
        let target_byte_pc = if target_idx as usize >= instr_to_byte_pc.len() {
            code_byte_len
        } else {
            instr_to_byte_pc[target_idx as usize]
        };
        let byte_delta = i64::from(target_byte_pc) - (i64::from(jump_byte_pc) + 1);
        let byte_delta_i32 =
            i32::try_from(byte_delta).expect("branch byte-offset delta exceeds i32 range");
        out[slot] = Operand::Imm32(byte_delta_i32);
    }
    out
}

/// One direct-eval caller binding: name → own-upvalue cell index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecDirectEvalBinding {
    /// Source-level binding name.
    pub(crate) name: Box<str>,
    /// Own-upvalue cell index inside the owning function's frame.
    pub(crate) upvalue: u16,
    /// `true` for `let` / `const` / `class` bindings.
    pub(crate) lexical: bool,
}

/// Compact mapped-arguments alias entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecMappedArgumentBinding {
    /// Argument object index.
    pub(crate) argument_index: u16,
    /// Storage backing the parameter binding.
    pub(crate) storage: ArgumentBindingStorage,
}

/// Hot dispatch instruction. Owns its operand slice so dispatch only
/// touches the instruction record and the per-instruction operand
/// allocation; there is no module-level side table to chase through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecInstr {
    /// Opcode.
    op: Op,
    /// Byte length of this instruction in the encoded stream
    /// (`opcode` + `operand_count` header + tagged operand bytes).
    /// `u16` to cover pathological inputs (constant-pool indices that
    /// occupy multiple varint bytes per operand combined with
    /// variadic opcodes) — a single instruction can encode up to
    /// ~640 bytes for `NewArray` over thousands of literals.
    byte_len: u16,
    /// Dense module-local property IC site id for named property ops.
    property_ic_site: u32,
    /// Byte-offset PC of this instruction in the encoded stream.
    byte_pc: u32,
    /// Operands in declaration order. Variadic opcodes (e.g. `Call`,
    /// `NewArray`, `MakeClosure`) just lengthen the slice; there is no
    /// fixed-width inline fast path.
    operands: Box<[Operand]>,
}

impl ExecInstr {
    fn from_operands(
        op: Op,
        operands: Vec<Operand>,
        property_ic_site: u32,
        byte_pc: u32,
        byte_len: u16,
    ) -> Self {
        Self {
            op,
            byte_len,
            property_ic_site,
            byte_pc,
            operands: operands.into_boxed_slice(),
        }
    }

    /// Opcode.
    #[must_use]
    pub(crate) const fn op(&self) -> Op {
        self.op
    }

    /// Byte length of this instruction in the encoded stream.
    #[must_use]
    pub(crate) const fn byte_len(&self) -> u32 {
        self.byte_len as u32
    }

    /// Byte-offset PC of this instruction in the encoded stream.
    #[must_use]
    pub(crate) const fn byte_pc(&self) -> u32 {
        self.byte_pc
    }

    /// Dense property IC site index for named property opcodes.
    #[must_use]
    pub(crate) const fn property_ic_site(&self) -> Option<usize> {
        if self.property_ic_site == NO_PROPERTY_IC_SITE {
            None
        } else {
            Some(self.property_ic_site as usize)
        }
    }

    fn operands(&self) -> &[Operand] {
        &self.operands
    }

    fn operand(&self, index: usize) -> Option<&Operand> {
        self.operands.get(index)
    }

    fn register(&self, index: usize) -> Option<u16> {
        match self.operand(index) {
            Some(Operand::Register(reg)) => Some(*reg),
            _ => None,
        }
    }

    fn const_index(&self, index: usize) -> Option<u32> {
        match self.operand(index) {
            Some(Operand::ConstIndex(idx)) => Some(*idx),
            _ => None,
        }
    }

    fn imm32(&self, index: usize) -> Option<i32> {
        match self.operand(index) {
            Some(Operand::Imm32(value)) => Some(*value),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{BytecodeModule, Instruction, SourceKind};

    fn function(code: Vec<Instruction>) -> Function {
        Function {
            id: 0,
            name: "exec-test".to_string(),
            code,
            ..Function::default()
        }
    }

    fn module(function: Function) -> BytecodeModule {
        BytecodeModule {
            module: "exec-test".to_string(),
            source_kind: SourceKind::JavaScript,
            functions: vec![function],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        }
    }

    #[test]
    fn fixed_width_operands_stay_inline() {
        let function = function(vec![Instruction {
            pc: 99,
            op: Op::Add,
            operands: vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2),
            ]
            .into(),
        }]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let instr = &executable.function(0).unwrap().code[0];

        assert_eq!(instr.op(), Op::Add);
        assert_eq!(executable.register(instr, 0), Some(0));
        assert_eq!(executable.register(instr, 1), Some(1));
        assert_eq!(executable.register(instr, 2), Some(2));
        assert_eq!(executable.register(instr, 3), None);
        assert_eq!(
            executable.operands(instr),
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2)
            ]
        );
    }

    #[test]
    fn variadic_operands_are_owned_by_the_instruction() {
        let operands = vec![
            Operand::Register(0),
            Operand::Register(1),
            Operand::ConstIndex(4),
            Operand::Register(2),
            Operand::Register(3),
        ];
        let function = function(vec![Instruction {
            pc: 7,
            op: Op::Call,
            operands: operands.clone().into(),
        }]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let instr = &executable.function(0).unwrap().code[0];

        assert_eq!(instr.op(), Op::Call);
        assert_eq!(executable.register(instr, 0), Some(0));
        assert_eq!(executable.register(instr, 1), Some(1));
        assert_eq!(executable.const_index(instr, 2), Some(4));
        assert_eq!(executable.register(instr, 3), Some(2));
        assert_eq!(executable.register(instr, 4), Some(3));
        assert_eq!(executable.register(instr, 5), None);
        assert_eq!(executable.operands(instr), operands.as_slice());
    }

    #[test]
    fn named_property_ops_get_dense_ic_sites() {
        let function = function(vec![
            Instruction {
                pc: 0,
                op: Op::LoadProperty,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                ]
                .into(),
            },
            Instruction {
                pc: 1,
                op: Op::StoreProperty,
                operands: vec![
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::Register(0),
                    Operand::Register(2),
                ]
                .into(),
            },
        ]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let function = executable.function(0).unwrap();

        assert_eq!(executable.property_ic_site_end(), 2);
        assert_eq!(function.code[0].property_ic_site(), Some(0));
        assert_eq!(function.code[1].property_ic_site(), Some(1));
    }

    #[test]
    fn builder_assigns_ic_sites_and_carries_variadic_operands() {
        let function = function(vec![
            Instruction {
                pc: 0,
                op: Op::LoadProperty,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                ]
                .into(),
            },
            Instruction {
                pc: 1,
                op: Op::Call,
                operands: vec![
                    Operand::Register(2),
                    Operand::Register(3),
                    Operand::ConstIndex(4),
                    Operand::Register(5),
                ]
                .into(),
            },
        ]);
        let module = module(function);

        let builder = ExecutableModuleBuilder::from_bytecode(&module);
        assert_eq!(builder.functions.len(), 1);
        assert_eq!(builder.next_property_ic_site, 1);

        let executable = builder.freeze();
        let exec_fn = executable.function(0).unwrap();
        assert_eq!(exec_fn.code.len(), 2);
        assert_eq!(executable.property_ic_site_end(), 1);
        assert_eq!(
            executable.operands(&exec_fn.code[1]),
            &[
                Operand::Register(2),
                Operand::Register(3),
                Operand::ConstIndex(4),
                Operand::Register(5)
            ]
        );
    }
}
