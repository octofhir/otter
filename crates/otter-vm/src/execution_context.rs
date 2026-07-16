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
//! - One context owns one chunk of the interpreter's shared
//!   [`crate::code_space::CodeSpace`]; function ids are global, and
//!   fid-keyed reads resolve foreign ids through the registry while
//!   constant-pool reads stay chunk-local.
//!
//! # See also
//!
//! - [`crate::code_space`]
//! - [`crate::microtask`]
//! - [`crate::timers`]
//! - [`crate::dynamic_import`]

use otter_bytecode::{BytecodeModule, Constant, Function, ModuleInit, Operand};
use std::sync::Arc;

use crate::code_space::{ChunkTables, CodeSpace, ResolvedCtx};
use crate::executable::{
    CodeBlock, CodeBlockInstruction, ExecutableModule, code_block_cfg::CodeBlockExceptionRegion,
};
use crate::property_atom::{AtomTable, AtomizedPropertyKey};

/// Cloneable dispatch context for VM-owned JS jobs.
pub struct ExecutionContext {
    module: Arc<BytecodeModule>,
    executable: Arc<ExecutableModule>,
    atoms: Arc<AtomTable>,
    /// First global function id owned by this chunk. Function-table
    /// lookups subtract this before indexing; ids below the base or
    /// past the table belong to sibling chunks in `space`.
    function_base: u32,
    /// Shared registry of every chunk linked into the owning
    /// interpreter. Foreign function ids (closures escaped from
    /// `eval` / `new Function` / other scripts) resolve through it.
    space: Arc<CodeSpace>,
}

impl std::fmt::Debug for ExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionContext")
            .field("module", &self.module.module)
            .field("function_base", &self.function_base())
            .finish_non_exhaustive()
    }
}

impl Clone for ExecutionContext {
    fn clone(&self) -> Self {
        Self {
            module: Arc::clone(&self.module),
            executable: Arc::clone(&self.executable),
            atoms: Arc::clone(&self.atoms),
            function_base: self.function_base,
            space: Arc::clone(&self.space),
        }
    }
}

impl ExecutionContext {
    /// Build a context from an owned bytecode module, linked into a
    /// fresh single-chunk code space. Embedders that execute several
    /// modules on one interpreter must use
    /// [`crate::Interpreter::link_module`] instead so all chunks share
    /// one function-id space.
    #[must_use]
    pub fn from_module(module: BytecodeModule) -> Self {
        Arc::new(CodeSpace::default()).link_module(module)
    }

    /// Wrap one linked chunk's tables. Only [`CodeSpace::link_module`] and
    /// [`Self::for_function`] construct contexts this way.
    #[must_use]
    pub(crate) fn from_chunk_tables(tables: ChunkTables, space: Arc<CodeSpace>) -> Self {
        Self {
            module: tables.module,
            executable: tables.executable,
            atoms: tables.atoms,
            function_base: tables.function_base,
            space,
        }
    }

    /// Resolve the immutable sibling chunk owning a foreign `function_id`.
    /// Registry nodes never move or disappear, so their tables borrow directly
    /// from the shared code-space chain without a per-context cache.
    fn sibling_tables(&self, function_id: u32) -> Option<&ChunkTables> {
        self.space.chunk_for(function_id)
    }

    /// First global function id owned by this chunk.
    #[must_use]
    pub(crate) fn function_base(&self) -> u32 {
        self.function_base
    }

    /// Stable cache key prefix for constant-pool values owned by this linked
    /// chunk. Separately linked standalone modules can both start at function
    /// id zero, so constant caches must key by the shared module allocation
    /// rather than by function id alone.
    #[must_use]
    pub(crate) fn constant_cache_key(&self, idx: u32) -> (usize, u32) {
        (Arc::as_ptr(&self.module) as usize, idx)
    }

    /// Shared code-space registry this chunk was linked into.
    #[must_use]
    pub(crate) fn space(&self) -> &Arc<CodeSpace> {
        &self.space
    }

    /// `true` when `function_id` falls inside this chunk's table.
    #[must_use]
    pub(crate) fn covers_function(&self, function_id: u32) -> bool {
        function_id
            .checked_sub(self.function_base)
            .is_some_and(|local| (local as usize) < self.module.functions.len())
    }

    /// Resolve the context owning `function_id`: this chunk on the hot
    /// in-chunk path, otherwise the sibling chunk registered in the
    /// shared code space. `None` means the id was never linked.
    #[must_use]
    pub(crate) fn for_function(&self, function_id: u32) -> Option<ResolvedCtx<'_>> {
        if self.covers_function(function_id) {
            return Some(ResolvedCtx::Ambient(self));
        }
        let tables = self.space.chunk_for(function_id)?.clone();
        Some(ResolvedCtx::Owned(Self::from_chunk_tables(
            tables,
            Arc::clone(&self.space),
        )))
    }

    /// Translate a global function id to this chunk's local table
    /// index.
    fn local_function_index(&self, function_id: u32) -> Option<u32> {
        let local = function_id.checked_sub(self.function_base)?;
        ((local as usize) < self.module.functions.len()).then_some(local)
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
    pub(crate) fn exec_main(&self) -> &CodeBlock {
        self.executable
            .function(0)
            .expect("bytecode modules always carry main function 0")
    }

    /// Tagged-template site descriptor (§13.2.8.4).
    #[must_use]
    pub fn template_site(&self, idx: u32) -> Option<&otter_bytecode::TemplateSite> {
        self.module.template_sites.get(idx as usize)
    }

    /// Resolve a tagged-template site in the chunk that owns `function_id`.
    ///
    /// Compiled frames may re-enter through an ambient sibling context, while
    /// template-site indices remain local to the frame's owning chunk.
    #[must_use]
    pub(crate) fn template_site_for_function(
        &self,
        function_id: u32,
        idx: u32,
    ) -> Option<&otter_bytecode::TemplateSite> {
        if self.local_function_index(function_id).is_some() {
            return self.module.template_sites.get(idx as usize);
        }
        self.sibling_tables(function_id)?
            .module
            .template_sites
            .get(idx as usize)
    }

    /// Return the stable linked-chunk base that owns `function_id`.
    #[must_use]
    pub(crate) fn function_base_for_function(&self, function_id: u32) -> Option<u32> {
        if self.local_function_index(function_id).is_some() {
            return Some(self.function_base);
        }
        Some(self.sibling_tables(function_id)?.function_base)
    }

    /// Module initialization records for linked module graphs.
    #[must_use]
    pub fn module_inits(&self) -> &[ModuleInit] {
        &self.module.module_inits
    }

    /// Function-table lookup by global VM function id. Foreign ids
    /// resolve transparently through the shared code space, so
    /// fid-keyed metadata reads (name, length, flags) work on any
    /// linked function value regardless of which chunk it escaped
    /// from.
    #[must_use]
    pub fn function(&self, function_id: u32) -> Option<&Function> {
        if let Some(local) = self.local_function_index(function_id) {
            return self.module.functions.get(local as usize);
        }
        let tables = self.sibling_tables(function_id)?;
        tables
            .module
            .functions
            .get((function_id - tables.function_base) as usize)
    }

    /// Executable function lookup by global VM function id. Foreign
    /// ids resolve like [`Self::function`]. Dispatch must still swap
    /// to the owning chunk's context (via [`Self::for_function`])
    /// before decoding constant-pool operands — only the function
    /// body itself is chunk-portable.
    #[must_use]
    pub(crate) fn exec_function(&self, function_id: u32) -> Option<&CodeBlock> {
        if let Some(local) = self.local_function_index(function_id) {
            return self.executable.function(local);
        }
        let tables = self.sibling_tables(function_id)?;
        tables
            .executable
            .function(function_id - tables.function_base)
    }

    fn code_block_arc(&self, function_id: u32) -> Option<std::sync::Arc<CodeBlock>> {
        if let Some(local) = self.local_function_index(function_id) {
            return self.executable.function_arc(local);
        }
        let tables = self.sibling_tables(function_id)?;
        tables
            .executable
            .function_arc(function_id - tables.function_base)
    }

    /// Build an owned JIT compile-input snapshot for a global VM function id.
    ///
    /// The returned DTO carries rewritten byte-PC branch deltas and dense
    /// property-IC site ids, matching the interpreter's executable view without
    /// exposing mutable VM execution state.
    #[must_use]
    pub fn jit_compile_snapshot(&self, function_id: u32) -> Option<crate::jit::JitCompileSnapshot> {
        let mut view = self.code_block_arc(function_id)?.jit_compile_snapshot();
        let code_block = Arc::clone(&view.code_block);
        // Mark each self-binding maker whose constant resolves to the function
        // being compiled: it materializes the named-function SELF binding, which
        // the emitter can read straight from the frame's own closure instead of
        // a Rust round-trip. Operand 1 is the function-id constant index.
        for instr in &mut view.instructions {
            if matches!(
                instr.op(&code_block),
                otter_bytecode::Op::MakeFunction | otter_bytecode::Op::MakeClosure
            ) && let Some(otter_bytecode::Operand::ConstIndex(idx)) =
                instr.operand(&code_block, 1)
                && self.function_id_constant(idx) == Some(function_id)
            {
                instr.make_self = true;
            }
            if instr.op(&code_block) == otter_bytecode::Op::LoadProperty
                && let Some(otter_bytecode::Operand::ConstIndex(idx)) =
                    instr.operand(&code_block, 2)
                && self
                    .string_constant_str_for_function(function_id, idx)
                    .is_some_and(|name| name == "length")
            {
                instr.load_array_length = true;
            }
            if instr.op(&code_block) == otter_bytecode::Op::CallMethodValue
                && let Some(otter_bytecode::Operand::ConstIndex(idx)) =
                    instr.operand(&code_block, 2)
                && let Some(name) = self.string_constant_str_for_function(function_id, idx)
            {
                instr.method_hint = match name {
                    "charCodeAt" => crate::jit::JitMethodHint::StringCharCodeAt,
                    "toString" => crate::jit::JitMethodHint::NumberToString,
                    _ => crate::jit::JitMethodHint::None,
                };
            }
            if instr.op(&code_block) == otter_bytecode::Op::LoadNumber
                && let Some(otter_bytecode::Operand::ConstIndex(idx)) =
                    instr.operand(&code_block, 1)
                && let Some(bits) = self.number_constant_bits(idx)
            {
                instr.load_number = Some(f64::from_bits(bits));
            }
        }
        Some(view)
    }

    /// Return one schema-typed operand.
    #[must_use]
    pub(crate) fn exec_operand(
        &self,
        instr: &CodeBlockInstruction,
        index: usize,
    ) -> Option<Operand> {
        self.exec_function(instr.code_block_id())?
            .operand(instr, index)
    }

    /// Decode one register operand.
    #[must_use]
    pub(crate) fn exec_register(&self, instr: &CodeBlockInstruction, index: usize) -> Option<u16> {
        match self.exec_operand(instr, index) {
            Some(Operand::Register(value)) => Some(value),
            _ => None,
        }
    }

    /// Decode the common `dst, lhs, rhs` register triple.
    #[must_use]
    pub(crate) fn exec_register3(&self, instr: &CodeBlockInstruction) -> Option<(u16, u16, u16)> {
        Some((
            self.exec_register(instr, 0)?,
            self.exec_register(instr, 1)?,
            self.exec_register(instr, 2)?,
        ))
    }

    /// Decode one constant-pool index operand.
    #[must_use]
    pub(crate) fn exec_const_index(
        &self,
        instr: &CodeBlockInstruction,
        index: usize,
    ) -> Option<u32> {
        match self.exec_operand(instr, index) {
            Some(Operand::ConstIndex(value)) => Some(value),
            _ => None,
        }
    }

    /// Decode one signed immediate operand.
    #[must_use]
    pub(crate) fn exec_imm32(&self, instr: &CodeBlockInstruction, index: usize) -> Option<i32> {
        match self.exec_operand(instr, index) {
            Some(Operand::Imm32(value)) => Some(value),
            _ => None,
        }
    }

    /// Pre-resolved static handlers owned by an `EnterTry` instruction.
    #[must_use]
    pub(crate) fn exec_exception_region(
        &self,
        instr: &CodeBlockInstruction,
    ) -> Option<CodeBlockExceptionRegion> {
        self.exec_function(instr.code_block_id())?
            .exception_region(instr.instruction_pc)
    }

    /// One past the highest dense named-property IC site id used by
    /// this chunk. Doubles as the interpreter IC-table capacity needed
    /// to dispatch this chunk.
    #[must_use]
    pub(crate) fn property_ic_site_end(&self) -> usize {
        self.executable.property_ic_site_end() as usize
    }

    /// Directory entries mapping this chunk's globally dense property site ids
    /// to typed payloads in their owning CodeBlock feedback vectors.
    pub(crate) fn feedback_slot_addresses(
        &self,
    ) -> Vec<(usize, crate::executable::FeedbackSlotAddress)> {
        self.executable.feedback_slot_addresses()
    }

    /// Dense property IC site for a named property instruction at the
    /// given canonical instruction-index PC.
    #[must_use]
    pub(crate) fn property_ic_site(&self, function_id: u32, pc: u32) -> Option<usize> {
        self.exec_function(function_id)?
            .instr_at_index(pc as usize)?
            .property_ic_site()
    }

    /// `true` when the function id points at an arrow function.
    #[must_use]
    pub fn function_is_arrow(&self, function_id: u32) -> bool {
        self.exec_function(function_id).is_some_and(|f| f.is_arrow)
    }

    /// `true` when this function id carries an implicit `prototype`
    /// own property: normal function declarations / expressions
    /// (§10.2.5 MakeConstructor), and every generator / async
    /// generator including generator *methods* (§15.5.5 / §15.6.5
    /// give each one a fresh `.prototype` for its instances).
    /// Arrows, non-generator methods, and plain async functions
    /// never receive one.
    #[must_use]
    pub fn function_has_prototype_property(&self, function_id: u32) -> bool {
        self.exec_function(function_id)
            .is_none_or(|f| f.is_generator || (!f.is_arrow && !f.is_method && !f.is_async))
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

    /// Resolve a string constant in the chunk that owns `function_id`.
    ///
    /// JIT runtime helpers execute with an ambient caller context, but compiled
    /// frames may belong to sibling chunks (Node shims, eval chunks, dynamic
    /// modules). Operand constant indices are local to the function's owning
    /// chunk, so method/property names must be resolved through that chunk's
    /// atom table rather than `self.atoms`.
    #[must_use]
    pub fn string_constant_str_for_function(&self, function_id: u32, idx: u32) -> Option<&str> {
        if self.local_function_index(function_id).is_some() {
            return self.atoms.string_constant_str(idx);
        }
        self.sibling_tables(function_id)?
            .atoms
            .string_constant_str(idx)
    }

    /// Resolve a string constant as an atomized property key.
    #[must_use]
    pub(crate) fn property_atom(&self, idx: u32) -> Option<AtomizedPropertyKey<'_>> {
        self.atoms.property_atom(idx)
    }

    /// Resolve an atomized property key in the chunk that owns `function_id`.
    #[must_use]
    pub(crate) fn property_atom_for_function(
        &self,
        function_id: u32,
        idx: u32,
    ) -> Option<AtomizedPropertyKey<'_>> {
        if self.local_function_index(function_id).is_some() {
            return self.atoms.property_atom(idx);
        }
        self.sibling_tables(function_id)?.atoms.property_atom(idx)
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

    /// Function id of a module's `<module-init>` by canonical URL.
    #[must_use]
    pub fn module_init_function_id(&self, url: &str) -> Option<u32> {
        self.module
            .module_inits
            .iter()
            .find(|m| m.url == url)
            .map(|m| m.function_id)
    }

    /// Canonical URLs of a module's non-deferred dependency targets.
    /// Runtime-added synthetic env-resolution edges and deferred edges are
    /// excluded, so the result is exactly the modules to evaluate before `url`.
    #[must_use]
    pub fn eager_dep_targets(&self, url: &str) -> Vec<&str> {
        self.module
            .module_resolutions
            .iter()
            .filter(|r| r.referrer == url && !r.deferred && !r.synthetic)
            .map(|r| r.target.as_str())
            .collect()
    }

    /// A module's `[[RequestedModules]]` in source order, with each
    /// edge's defer phase. `import("x")` preload edges (synthetic
    /// `deferred && dynamic`) are not source requests and are
    /// excluded, as are runtime synthetic env-resolution edges.
    #[must_use]
    pub fn module_requests(&self, url: &str) -> Vec<(&str, bool)> {
        self.module
            .module_resolutions
            .iter()
            .filter(|r| r.referrer == url && !r.synthetic && !(r.deferred && r.dynamic))
            .map(|r| (r.target.as_str(), r.deferred))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{
        ArgumentsObjectKind, BytecodeModule, Constant, Function, Instruction, Op, Operand,
        SourceKind,
    };

    use super::ExecutionContext;
    use crate::{Interpreter, Value};

    fn instr(pc: u32, op: Op, operands: impl AsRef<[Operand]>) -> Instruction {
        Instruction {
            pc,
            op,
            operands: operands.as_ref().to_vec(),
        }
    }

    fn module_with(
        code: Vec<Instruction>,
        constants: Vec<Constant>,
        scratch: u16,
    ) -> BytecodeModule {
        BytecodeModule {
            module: "<test>".to_string(),
            template_sites: Vec::new(),
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
                is_method: false,
                has_rest: false,
                is_async: false,
                is_generator: false,
                is_async_generator: false,
                is_derived_constructor: false,
                is_module: false,
                needs_arguments: false,
                uses_arguments_callee: false,
                arguments_object_kind: ArgumentsObjectKind::Unmapped,
                mapped_argument_bindings: Vec::new(),
                source_text: None,
                source_text_span: None,
                module_url: String::new(),
                direct_eval_bindings: Vec::new(),
                contains_direct_eval: false,
                code: code.into(),
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
    fn jit_snapshot_marks_array_length_loads() {
        let context = ExecutionContext::from_module(module_with(
            vec![
                instr(
                    0,
                    Op::LoadProperty,
                    [
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::ConstIndex(0),
                    ],
                ),
                instr(
                    1,
                    Op::LoadProperty,
                    [
                        Operand::Register(2),
                        Operand::Register(1),
                        Operand::ConstIndex(1),
                    ],
                ),
            ],
            vec![string_constant("length"), string_constant("value")],
            3,
        ));

        let view = context.jit_compile_snapshot(0).expect("function exists");

        assert!(view.instructions[0].load_array_length);
        assert!(!view.instructions[1].load_array_length);
    }

    #[test]
    fn jit_snapshot_marks_primitive_method_hints() {
        let context = ExecutionContext::from_module(module_with(
            vec![
                instr(
                    0,
                    Op::CallMethodValue,
                    [
                        Operand::Register(0),
                        Operand::Register(1),
                        Operand::ConstIndex(0),
                        Operand::ConstIndex(1),
                        Operand::Register(2),
                    ],
                ),
                instr(
                    1,
                    Op::CallMethodValue,
                    [
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::ConstIndex(2),
                        Operand::ConstIndex(0),
                    ],
                ),
            ],
            vec![
                string_constant("charCodeAt"),
                string_constant("unused"),
                string_constant("value"),
            ],
            4,
        ));

        let view = context.jit_compile_snapshot(0).expect("function exists");

        assert_eq!(
            view.instructions[0].method_hint,
            crate::jit::JitMethodHint::StringCharCodeAt
        );
        assert_eq!(
            view.instructions[1].method_hint,
            crate::jit::JitMethodHint::None
        );
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
