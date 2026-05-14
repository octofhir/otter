//! Frozen execution bytecode for the VM dispatch loop.
//!
//! `otter-bytecode` owns the compiler/debug DTO shape. The VM owns this
//! compact view so hot dispatch can stop reading per-instruction `pc`
//! fields and avoid following heap operand vectors for fixed-width
//! instructions.
//!
//! # Contents
//! - [`ExecutableModule`] — VM-owned frozen function table.
//! - [`ExecutableFunction`] — one function's compact instruction stream.
//! - [`ExecInstr`] — hot instruction record with inline operands.
//!
//! # Invariants
//! - `ExecInstr` never stores a program counter; the vector index is the PC.
//! - Instructions with three or fewer operands store them inline.
//! - Variadic instructions store operands in the module side table and carry
//!   only a compact span into that table.
//! - Named property IC sites get dense VM-local ids during executable
//!   construction; bytecode JSON stays unchanged.
//!
//! # See also
//! - [`crate::execution_context`]
//! - [`otter_bytecode::Instruction`]

use otter_bytecode::{
    ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, Function, Op, Operand,
};

const INLINE_OPERANDS: usize = 3;
const EMPTY_OPERAND: Operand = Operand::Imm32(0);
const NO_PROPERTY_IC_SITE: u32 = u32::MAX;

/// VM-owned executable view of a bytecode module.
#[derive(Debug, Clone)]
pub(crate) struct ExecutableModule {
    functions: Box<[ExecutableFunction]>,
    side_operands: Box<[Operand]>,
    property_ic_site_count: u32,
}

impl ExecutableModule {
    /// Build a frozen execution view from the compiler/debug module DTO.
    #[must_use]
    pub(crate) fn from_bytecode(module: &BytecodeModule) -> Self {
        let mut side_operands = Vec::new();
        let mut next_property_ic_site = 0_u32;
        let functions = module
            .functions
            .iter()
            .map(|function| {
                ExecutableFunction::from_bytecode(
                    function,
                    &mut side_operands,
                    &mut next_property_ic_site,
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            functions,
            side_operands: side_operands.into_boxed_slice(),
            property_ic_site_count: next_property_ic_site,
        }
    }

    /// Function-table lookup by VM function id.
    #[must_use]
    pub(crate) fn function(&self, function_id: u32) -> Option<&ExecutableFunction> {
        self.functions.get(function_id as usize)
    }

    /// Return an instruction's operands in declaration order.
    #[must_use]
    pub(crate) fn operands<'a>(&'a self, instr: &'a ExecInstr) -> &'a [Operand] {
        instr.operands(&self.side_operands)
    }

    /// Return one instruction operand by index without materialising the
    /// whole operand slice at the call site.
    #[must_use]
    pub(crate) fn operand<'a>(&'a self, instr: &'a ExecInstr, index: usize) -> Option<&'a Operand> {
        instr.operand(&self.side_operands, index)
    }

    /// Decode one register operand.
    #[must_use]
    pub(crate) fn register(&self, instr: &ExecInstr, index: usize) -> Option<u16> {
        instr.register(&self.side_operands, index)
    }

    /// Decode one constant-pool index operand.
    #[must_use]
    pub(crate) fn const_index(&self, instr: &ExecInstr, index: usize) -> Option<u32> {
        instr.const_index(&self.side_operands, index)
    }

    /// Decode one signed immediate operand.
    #[must_use]
    pub(crate) fn imm32(&self, instr: &ExecInstr, index: usize) -> Option<i32> {
        instr.imm32(&self.side_operands, index)
    }

    /// Number of dense named-property IC sites in this module.
    #[must_use]
    pub(crate) const fn property_ic_site_count(&self) -> u32 {
        self.property_ic_site_count
    }
}

/// One executable function body.
#[derive(Debug, Clone)]
pub(crate) struct ExecutableFunction {
    /// Index into the executable function table.
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
    /// `true` when this function declares a rest parameter.
    pub(crate) has_rest: bool,
    /// `true` when this function is async.
    pub(crate) is_async: bool,
    /// `true` when this function is a generator.
    pub(crate) is_generator: bool,
    /// `true` when this function is an async generator.
    pub(crate) is_async_generator: bool,
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
    /// Compact instruction stream. The index is the program counter.
    pub(crate) code: Box<[ExecInstr]>,
}

impl ExecutableFunction {
    fn from_bytecode(
        function: &Function,
        side_operands: &mut Vec<Operand>,
        next_property_ic_site: &mut u32,
    ) -> Self {
        let register_count = function
            .param_count
            .saturating_add(function.locals)
            .saturating_add(function.scratch);
        let code = function
            .code
            .iter()
            .map(|instr| {
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
                ExecInstr::from_operands(
                    instr.op,
                    instr.operands.as_slice(),
                    side_operands,
                    property_ic_site,
                )
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
        Self {
            id: function.id,
            param_count: function.param_count,
            register_count,
            own_upvalue_count: function.own_upvalue_count,
            is_strict: function.is_strict,
            is_arrow: function.is_arrow,
            has_rest: function.has_rest,
            is_async: function.is_async,
            is_generator: function.is_generator,
            is_async_generator: function.is_async_generator,
            needs_arguments: function.needs_arguments,
            arguments_object_kind: function.arguments_object_kind,
            mapped_argument_bindings,
            is_module: function.is_module,
            module_url: function.module_url.clone().into_boxed_str(),
            code,
        }
    }
}

/// Compact mapped-arguments alias entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecMappedArgumentBinding {
    /// Argument object index.
    pub(crate) argument_index: u16,
    /// Storage backing the parameter binding.
    pub(crate) storage: ArgumentBindingStorage,
}

/// Hot dispatch instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecInstr {
    /// Opcode.
    op: Op,
    /// Operand count. Values greater than three read from the side table.
    operand_len: u8,
    /// Inline operand slots for common fixed-width instructions.
    inline_operands: [Operand; INLINE_OPERANDS],
    /// Start index in [`ExecutableModule::side_operands`] for variadic ops.
    side_start: u32,
    /// Dense module-local property IC site id for named property ops.
    property_ic_site: u32,
}

impl ExecInstr {
    fn from_operands(
        op: Op,
        operands: &[Operand],
        side_operands: &mut Vec<Operand>,
        property_ic_site: u32,
    ) -> Self {
        let operand_len =
            u8::try_from(operands.len()).expect("instruction operand count exceeds u8");
        if operands.len() <= INLINE_OPERANDS {
            let mut inline_operands = [EMPTY_OPERAND; INLINE_OPERANDS];
            inline_operands[..operands.len()].copy_from_slice(operands);
            Self {
                op,
                operand_len,
                inline_operands,
                side_start: 0,
                property_ic_site,
            }
        } else {
            let side_start = u32::try_from(side_operands.len())
                .expect("executable side operand table too large");
            side_operands.extend_from_slice(operands);
            Self {
                op,
                operand_len,
                inline_operands: [EMPTY_OPERAND; INLINE_OPERANDS],
                side_start,
                property_ic_site,
            }
        }
    }

    /// Opcode.
    #[must_use]
    pub(crate) const fn op(&self) -> Op {
        self.op
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

    fn operands<'a>(&'a self, side_operands: &'a [Operand]) -> &'a [Operand] {
        let len = self.operand_len as usize;
        if len <= INLINE_OPERANDS {
            &self.inline_operands[..len]
        } else {
            let start = self.side_start as usize;
            &side_operands[start..start + len]
        }
    }

    fn operand<'a>(&'a self, side_operands: &'a [Operand], index: usize) -> Option<&'a Operand> {
        if index >= self.operand_len as usize {
            return None;
        }
        if self.operand_len as usize <= INLINE_OPERANDS {
            self.inline_operands.get(index)
        } else {
            side_operands.get(self.side_start as usize + index)
        }
    }

    fn register(&self, side_operands: &[Operand], index: usize) -> Option<u16> {
        match self.operand(side_operands, index) {
            Some(Operand::Register(reg)) => Some(*reg),
            _ => None,
        }
    }

    fn const_index(&self, side_operands: &[Operand], index: usize) -> Option<u32> {
        match self.operand(side_operands, index) {
            Some(Operand::ConstIndex(idx)) => Some(*idx),
            _ => None,
        }
    }

    fn imm32(&self, side_operands: &[Operand], index: usize) -> Option<i32> {
        match self.operand(side_operands, index) {
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
        assert_eq!(instr.side_start, 0);
        assert!(executable.side_operands.is_empty());
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
    fn variadic_operands_use_side_table() {
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
        assert_eq!(executable.side_operands.as_ref(), operands.as_slice());
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

        assert_eq!(executable.property_ic_site_count(), 2);
        assert_eq!(function.code[0].property_ic_site(), Some(0));
        assert_eq!(function.code[1].property_ic_site(), Some(1));
    }
}
