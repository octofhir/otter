//! Isolate-local execution context handles.
//!
//! A queued JS job must know which compiled code table owns its
//! callback. Production engines get that from their internal
//! function / realm / module records; Otter keeps the same boundary
//! explicit with this small handle.
//!
//! # Contents
//!
//! - [`ExecutionContext`] — cloneable VM-owned dispatch context.
//!
//! # Invariants
//!
//! - Host schedulers never receive an [`ExecutionContext`]. They
//!   only see opaque tokens and wake the isolate.
//! - JS-visible work queued inside the VM carries its context with
//!   the job, so dispatch never depends on ambient runtime state.
//! - The bytecode module is an implementation detail of the
//!   context. Callers use narrow accessors for function-table,
//!   constant-pool, and module-resolution reads.
//!
//! # See also
//!
//! - [`crate::microtask`]
//! - [`crate::timers`]
//! - [`crate::dynamic_import`]

use otter_bytecode::{BytecodeModule, Constant, Function, ModuleInit, Operand};

use crate::executable::{ExecInstr, ExecutableFunction, ExecutableModule};
use crate::property_atom::{AtomTable, AtomizedPropertyKey};

/// Cloneable dispatch context for VM-owned JS jobs.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    module: std::rc::Rc<BytecodeModule>,
    executable: std::rc::Rc<ExecutableModule>,
    atoms: std::rc::Rc<AtomTable>,
}

impl ExecutionContext {
    /// Build a context from an owned bytecode module.
    #[must_use]
    pub fn from_module(module: BytecodeModule) -> Self {
        let executable = ExecutableModule::from_bytecode(&module);
        let atoms = AtomTable::from_constants(&module.constants);
        Self {
            module: std::rc::Rc::new(module),
            executable: std::rc::Rc::new(executable),
            atoms: std::rc::Rc::new(atoms),
        }
    }

    /// Synthetic bytecode module name.
    #[must_use]
    pub fn module_name(&self) -> &str {
        &self.module.module
    }

    /// Entry function for a script/module turn.
    #[must_use]
    pub fn main(&self) -> &Function {
        self.module.main()
    }

    /// Entry executable function for a script/module turn.
    #[must_use]
    pub(crate) fn exec_main(&self) -> &ExecutableFunction {
        self.executable
            .function(0)
            .expect("bytecode modules always carry main function 0")
    }

    /// Module initialization records for linked module graphs.
    #[must_use]
    pub fn module_inits(&self) -> &[ModuleInit] {
        &self.module.module_inits
    }

    /// Function-table lookup by VM function id.
    #[must_use]
    pub fn function(&self, function_id: u32) -> Option<&Function> {
        self.module.functions.get(function_id as usize)
    }

    /// Executable function lookup by VM function id.
    #[must_use]
    pub(crate) fn exec_function(&self, function_id: u32) -> Option<&ExecutableFunction> {
        self.executable.function(function_id)
    }

    /// Return an executable instruction's operands in declaration order.
    #[must_use]
    pub(crate) fn exec_operands<'a>(&'a self, instr: &'a ExecInstr) -> &'a [Operand] {
        self.executable.operands(instr)
    }

    /// Return one executable instruction operand by index.
    #[must_use]
    pub(crate) fn exec_operand<'a>(
        &'a self,
        instr: &'a ExecInstr,
        index: usize,
    ) -> Option<&'a Operand> {
        self.executable.operand(instr, index)
    }

    /// Decode one executable register operand.
    #[must_use]
    pub(crate) fn exec_register(&self, instr: &ExecInstr, index: usize) -> Option<u16> {
        self.executable.register(instr, index)
    }

    /// Decode the common `dst, lhs, rhs` register triple.
    #[must_use]
    pub(crate) fn exec_register3(&self, instr: &ExecInstr) -> Option<(u16, u16, u16)> {
        Some((
            self.exec_register(instr, 0)?,
            self.exec_register(instr, 1)?,
            self.exec_register(instr, 2)?,
        ))
    }

    /// Decode one executable constant-pool index operand.
    #[must_use]
    pub(crate) fn exec_const_index(&self, instr: &ExecInstr, index: usize) -> Option<u32> {
        self.executable.const_index(instr, index)
    }

    /// Decode one executable signed immediate operand.
    #[must_use]
    pub(crate) fn exec_imm32(&self, instr: &ExecInstr, index: usize) -> Option<i32> {
        self.executable.imm32(instr, index)
    }

    /// Number of dense named-property IC sites in this context.
    #[must_use]
    pub(crate) fn property_ic_site_count(&self) -> usize {
        self.executable.property_ic_site_count() as usize
    }

    /// Dense property IC site for a named property instruction at the
    /// given byte-offset PC.
    #[must_use]
    pub(crate) fn property_ic_site(&self, function_id: u32, pc: u32) -> Option<usize> {
        self.exec_function(function_id)?
            .instr_at_byte_pc(pc)?
            .property_ic_site()
    }

    /// `true` when the function id points at an arrow function.
    #[must_use]
    pub fn function_is_arrow(&self, function_id: u32) -> bool {
        self.exec_function(function_id).is_some_and(|f| f.is_arrow)
    }

    /// `true` when the function id points at a strict function.
    #[must_use]
    pub fn function_is_strict(&self, function_id: u32) -> bool {
        self.exec_function(function_id).is_some_and(|f| f.is_strict)
    }

    /// Resolve a function-id constant.
    #[must_use]
    pub fn function_id_constant(&self, idx: u32) -> Option<u32> {
        match self.module.constants.get(idx as usize) {
            Some(Constant::FunctionId { index }) => Some(*index),
            _ => None,
        }
    }

    /// Resolve a string constant as WTF-16 code units.
    #[must_use]
    pub fn string_constant_units(&self, idx: u32) -> Option<&[u16]> {
        match self.module.constants.get(idx as usize) {
            Some(Constant::String { utf16 }) => Some(utf16.as_slice()),
            _ => None,
        }
    }

    /// Resolve a string constant as a borrowed UTF-8 string.
    #[must_use]
    pub fn string_constant_str(&self, idx: u32) -> Option<&str> {
        self.atoms.string_constant_str(idx)
    }

    /// Resolve a string constant as an atomized property key.
    #[must_use]
    pub(crate) fn property_atom(&self, idx: u32) -> Option<AtomizedPropertyKey<'_>> {
        self.atoms.property_atom(idx)
    }

    /// Resolve a numeric constant's raw IEEE-754 bits.
    #[must_use]
    pub fn number_constant_bits(&self, idx: u32) -> Option<u64> {
        match self.module.constants.get(idx as usize) {
            Some(Constant::Number { bits }) => Some(*bits),
            _ => None,
        }
    }

    /// Resolve a BigInt decimal literal constant.
    #[must_use]
    pub fn bigint_decimal_constant(&self, idx: u32) -> Option<&str> {
        match self.module.constants.get(idx as usize) {
            Some(Constant::BigInt { decimal }) => Some(decimal.as_str()),
            _ => None,
        }
    }

    /// Resolve a RegExp literal constant.
    #[must_use]
    pub fn regexp_constant(&self, idx: u32) -> Option<(&[u16], &str)> {
        match self.module.constants.get(idx as usize) {
            Some(Constant::RegExp {
                pattern_utf16,
                flags,
            }) => Some((pattern_utf16.as_slice(), flags.as_str())),
            _ => None,
        }
    }

    /// Resolve a module import edge from the bytecode resolution table.
    #[must_use]
    pub fn module_resolution_target(&self, referrer: &str, specifier: &str) -> Option<&str> {
        self.module
            .module_resolutions
            .iter()
            .find(|r| r.referrer == referrer && r.specifier == specifier)
            .map(|r| r.target.as_str())
    }
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{
        ArgumentsObjectKind, BytecodeModule, Constant, Function, Instruction, Op, Operand,
        OperandList, SourceKind,
    };

    use super::ExecutionContext;
    use crate::{Interpreter, Value};

    fn instr(pc: u32, op: Op, operands: impl Into<OperandList>) -> Instruction {
        Instruction {
            pc,
            op,
            operands: operands.into(),
        }
    }

    fn module_with(
        code: Vec<Instruction>,
        constants: Vec<Constant>,
        scratch: u16,
    ) -> BytecodeModule {
        BytecodeModule {
            module: "<test>".to_string(),
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".to_string(),
                span: (0, 0),
                locals: 0,
                scratch,
                param_count: 0,
                length: 0,
                own_upvalue_count: 0,
                is_strict: false,
                is_arrow: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_module: false,
                needs_arguments: false,
                arguments_object_kind: ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                module_url: String::new(),
                code,
                spans: Vec::new(),
            }],
            constants,
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        }
    }

    fn string_constant(text: &str) -> Constant {
        Constant::String {
            utf16: text.encode_utf16().collect(),
        }
    }

    fn run_module(module: BytecodeModule) -> Value {
        run_module_with_interpreter(module).0
    }

    fn run_module_with_interpreter(module: BytecodeModule) -> (Value, Interpreter) {
        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();
        let value = interp.run(&context).expect("test bytecode runs");
        (value, interp)
    }

    #[test]
    fn string_constants_have_stable_property_atoms() {
        let context = ExecutionContext::from_module(module_with(
            vec![instr(0, Op::ReturnUndefined, [])],
            vec![string_constant("foo")],
            1,
        ));

        let key = context
            .property_atom(0)
            .expect("string constant is atomized");
        assert_eq!(key.name(), "foo");
        assert!(context.property_atom(1).is_none());
    }

    #[test]
    fn atomized_named_store_then_load_keeps_semantics() {
        let module = module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(1, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    2,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                instr(
                    3,
                    Op::LoadProperty,
                    [
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(4, Op::Return, [Operand::Register(3)]),
            ],
            vec![string_constant("foo")],
            4,
        );

        assert_eq!(run_module(module), Value::boolean(true));
    }

    #[test]
    fn named_load_installs_ordinary_own_data_ic() {
        let module = module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(1, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    2,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                instr(
                    3,
                    Op::LoadProperty,
                    [
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(4, Op::Return, [Operand::Register(3)]),
            ],
            vec![string_constant("foo")],
            4,
        );

        let (value, interp) = run_module_with_interpreter(module);

        assert_eq!(value, Value::boolean(true));
        assert_eq!(interp.load_property_ic_count(), 1);
    }

    #[test]
    fn named_store_installs_ordinary_own_data_ic() {
        let module = module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(1, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    2,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                instr(
                    3,
                    Op::LoadProperty,
                    [
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(4, Op::Return, [Operand::Register(3)]),
            ],
            vec![string_constant("foo")],
            4,
        );

        let (value, interp) = run_module_with_interpreter(module);

        assert_eq!(value, Value::boolean(true));
        assert_eq!(interp.store_property_ic_count(), 1);
    }

    #[test]
    fn property_ic_stats_record_load_hit_after_warmup() {
        let context = ExecutionContext::from_module(module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(1, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    2,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                instr(
                    3,
                    Op::LoadProperty,
                    [
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(4, Op::Return, [Operand::Register(3)]),
            ],
            vec![string_constant("foo")],
            4,
        ));
        let mut interp = Interpreter::new();

        assert_eq!(
            interp.run(&context).expect("first run"),
            Value::boolean(true)
        );
        assert_eq!(
            interp.run(&context).expect("second run"),
            Value::boolean(true)
        );

        let stats = interp.property_ic_stats();
        assert_eq!(stats.load_hits, 1);
        assert_eq!(stats.load_misses, 1);
        assert_eq!(stats.load_installs, 1);
    }

    #[test]
    fn property_ic_stats_record_direct_prototype_load_hit_after_warmup() {
        let context = ExecutionContext::from_module(module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(1, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    2,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                instr(3, Op::NewObject, [Operand::Register(3)]),
                instr(
                    4,
                    Op::SetPrototype,
                    [Operand::Register(3), Operand::Register(0)],
                ),
                instr(
                    5,
                    Op::LoadProperty,
                    [
                        Operand::Register(4),
                        Operand::Register(3),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(6, Op::Return, [Operand::Register(4)]),
            ],
            vec![string_constant("foo")],
            5,
        ));
        let mut interp = Interpreter::new();

        assert_eq!(
            interp.run(&context).expect("first run"),
            Value::boolean(true)
        );
        assert_eq!(
            interp.run(&context).expect("second run"),
            Value::boolean(true)
        );

        let stats = interp.property_ic_stats();
        assert_eq!(stats.load_hits, 1);
        assert_eq!(stats.load_misses, 1);
        assert_eq!(stats.load_installs, 1);
    }

    #[test]
    fn property_ic_stats_record_has_property_hit_after_warmup() {
        let context = ExecutionContext::from_module(module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(
                    1,
                    Op::LoadString,
                    [Operand::Register(7), Operand::ConstIndex(0)],
                ),
                instr(2, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    3,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(8),
                    ],
                ),
                instr(
                    4,
                    Op::HasProperty,
                    [
                        Operand::Register(4),
                        Operand::Register(7),
                        Operand::Register(0),
                    ],
                ),
                instr(5, Op::Return, [Operand::Register(4)]),
            ],
            vec![string_constant("foo")],
            9,
        ));
        let mut interp = Interpreter::new();

        assert_eq!(
            interp.run(&context).expect("first run"),
            Value::boolean(true)
        );
        assert_eq!(
            interp.run(&context).expect("second run"),
            Value::boolean(true)
        );

        let stats = interp.property_ic_stats();
        assert_eq!(stats.has_hits, 1);
        assert_eq!(stats.has_misses, 1);
        assert_eq!(stats.has_installs, 1);
    }

    #[test]
    fn property_ic_stats_record_direct_prototype_has_property_hit_after_warmup() {
        let context = ExecutionContext::from_module(module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(
                    1,
                    Op::LoadString,
                    [Operand::Register(7), Operand::ConstIndex(0)],
                ),
                instr(2, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    3,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(8),
                    ],
                ),
                instr(4, Op::NewObject, [Operand::Register(3)]),
                instr(
                    5,
                    Op::SetPrototype,
                    [Operand::Register(3), Operand::Register(0)],
                ),
                instr(
                    6,
                    Op::HasProperty,
                    [
                        Operand::Register(4),
                        Operand::Register(7),
                        Operand::Register(3),
                    ],
                ),
                instr(7, Op::Return, [Operand::Register(4)]),
            ],
            vec![string_constant("foo")],
            9,
        ));
        let mut interp = Interpreter::new();

        assert_eq!(
            interp.run(&context).expect("first run"),
            Value::boolean(true)
        );
        assert_eq!(
            interp.run(&context).expect("second run"),
            Value::boolean(true)
        );

        let stats = interp.property_ic_stats();
        assert_eq!(stats.has_hits, 1);
        assert_eq!(stats.has_misses, 1);
        assert_eq!(stats.has_installs, 1);
    }

    #[test]
    fn property_ic_stats_record_store_hit_on_same_site() {
        let module = module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(1, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    2,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(4),
                    ],
                ),
                instr(
                    3,
                    Op::LoadProperty,
                    [
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(4, Op::Return, [Operand::Register(3)]),
            ],
            vec![string_constant("foo")],
            5,
        );

        let context = ExecutionContext::from_module(module);
        let mut interp = Interpreter::new();

        assert_eq!(
            interp.run(&context).expect("first run"),
            Value::boolean(true)
        );
        assert_eq!(
            interp.run(&context).expect("second run"),
            Value::boolean(true)
        );
        let stats = interp.property_ic_stats();
        assert_eq!(stats.store_hits, 1);
        assert_eq!(stats.store_misses, 1);
        assert_eq!(stats.store_installs, 1);
    }

    #[test]
    fn deleted_object_shape_does_not_install_later_store_ic() {
        let context = ExecutionContext::from_module(module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(1, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    2,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                instr(
                    3,
                    Op::DeleteProperty,
                    [
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(
                    4,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                instr(
                    5,
                    Op::LoadProperty,
                    [
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(6, Op::Return, [Operand::Register(4)]),
            ],
            vec![string_constant("foo")],
            5,
        ));
        let mut interp = Interpreter::new();

        assert_eq!(
            interp.run(&context).expect("first run"),
            Value::boolean(true)
        );
        assert_eq!(
            interp.run(&context).expect("second run"),
            Value::boolean(true)
        );

        let stats = interp.property_ic_stats();
        assert_eq!(stats.store_installs, 1);
        assert_eq!(interp.store_property_ic_count(), 1);
    }

    #[test]
    fn deleted_object_shape_does_not_install_later_load_ic() {
        let context = ExecutionContext::from_module(module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(1, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    2,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                instr(
                    3,
                    Op::DeleteProperty,
                    [
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(
                    4,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(1),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                instr(
                    5,
                    Op::LoadProperty,
                    [
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::ConstIndex(1),
                    ],
                ),
                instr(6, Op::Return, [Operand::Register(4)]),
            ],
            vec![string_constant("foo"), string_constant("bar")],
            5,
        ));
        let mut interp = Interpreter::new();

        assert_eq!(
            interp.run(&context).expect("first run"),
            Value::boolean(true)
        );
        assert_eq!(
            interp.run(&context).expect("second run"),
            Value::boolean(true)
        );

        assert_eq!(interp.load_property_ic_count(), 0);
    }

    #[test]
    fn atomized_named_delete_removes_property() {
        let module = module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(1, Op::LoadTrue, [Operand::Register(1)]),
                instr(
                    2,
                    Op::StoreProperty,
                    [
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                instr(
                    3,
                    Op::DeleteProperty,
                    [
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(
                    4,
                    Op::LoadProperty,
                    [
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(5, Op::LoadUndefined, [Operand::Register(5)]),
                instr(
                    6,
                    Op::Equal,
                    [
                        Operand::Register(6),
                        Operand::Register(4),
                        Operand::Register(5),
                    ],
                ),
                instr(7, Op::Return, [Operand::Register(6)]),
            ],
            vec![string_constant("foo")],
            7,
        );

        assert_eq!(run_module(module), Value::boolean(true));
    }

    #[test]
    fn computed_string_property_path_is_unchanged() {
        let module = module_with(
            vec![
                instr(0, Op::NewObject, [Operand::Register(0)]),
                instr(
                    1,
                    Op::LoadString,
                    [Operand::Register(1), Operand::ConstIndex(0)],
                ),
                instr(2, Op::LoadTrue, [Operand::Register(2)]),
                instr(
                    3,
                    Op::StoreElement,
                    [
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::Register(2),
                        Operand::Register(4),
                    ],
                ),
                instr(
                    4,
                    Op::LoadElement,
                    [
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                instr(5, Op::Return, [Operand::Register(3)]),
            ],
            vec![string_constant("foo")],
            5,
        );

        assert_eq!(run_module(module), Value::boolean(true));
    }
}
