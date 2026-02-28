//! Bytecode generation from AST

use otter_vm_bytecode::{
    ConstantIndex, ConstantPool, Function, Instruction, JumpOffset, Module, Register,
    UpvalueCapture,
    function::{FunctionBuilder, FunctionFlags, SourceMap},
    module::{ExportRecord, ImportRecord},
};

use crate::error::{CompileError, CompileResult};
use crate::peephole::PeepholeOptimizer;
use crate::scope::{ResolvedBinding, ScopeChain, VariableKind};
use otter_vm_bytecode::Constant;

/// Register allocator
#[derive(Debug)]
pub struct RegisterAllocator {
    /// Next available register
    next: u16,
    /// Maximum register used
    max: u16,
    /// Free-list of registers that can be reused.
    free: Vec<u16>,
    /// Tracks which registers are currently allocated (debug-only).
    #[cfg(debug_assertions)]
    in_use: Vec<bool>,
}

impl RegisterAllocator {
    /// Create a new register allocator
    pub fn new() -> Self {
        Self {
            next: 0,
            max: 0,
            free: Vec::new(),
            #[cfg(debug_assertions)]
            in_use: vec![false; 65536],
        }
    }

    /// Allocate a register
    pub fn alloc(&mut self) -> Register {
        if let Some(id) = self.free.pop() {
            #[cfg(debug_assertions)]
            {
                debug_assert!(!self.in_use[id as usize], "register {id} already in use");
                self.in_use[id as usize] = true;
            }
            Register(id)
        } else {
            let reg = Register(self.next);
            #[cfg(debug_assertions)]
            {
                debug_assert!(
                    !self.in_use[reg.0 as usize],
                    "register {} already in use",
                    reg.0
                );
                self.in_use[reg.0 as usize] = true;
            }
            self.next = self
                .next
                .checked_add(1)
                .expect("register allocation overflow");
            self.max = self.max.max(self.next);
            reg
        }
    }

    /// Allocate a contiguous block of new registers (does not use the free-list).
    ///
    /// This is used for the calling convention where `func`, `func+1..func+argc`
    /// must be contiguous.
    pub fn alloc_fresh_block(&mut self, count: u8) -> Register {
        let base = self.next;
        #[cfg(debug_assertions)]
        for id in base..base.saturating_add(count as u16) {
            debug_assert!(!self.in_use[id as usize], "register {id} already in use");
            self.in_use[id as usize] = true;
        }
        self.next = self
            .next
            .checked_add(count as u16)
            .expect("register allocation overflow");
        self.max = self.max.max(self.next);
        Register(base)
    }

    /// Free a register
    pub fn free(&mut self, reg: Register) {
        #[cfg(debug_assertions)]
        {
            debug_assert!(
                self.in_use[reg.0 as usize],
                "freeing register {} that is not in use",
                reg.0
            );
            self.in_use[reg.0 as usize] = false;
        }
        self.free.push(reg.0);
    }

    /// Get current position (for restoring later)
    pub fn position(&self) -> u16 {
        self.next
    }

    /// Restore to a previous position
    pub fn restore(&mut self, pos: u16) {
        #[cfg(debug_assertions)]
        for id in pos..self.next {
            self.in_use[id as usize] = false;
        }
        self.next = pos;
        self.free.clear();
    }

    /// Get maximum registers used
    pub fn max_used(&self) -> u16 {
        self.max
    }
}

impl Default for RegisterAllocator {
    fn default() -> Self {
        Self::new()
    }
}

/// Function being compiled
#[derive(Debug)]
pub struct FunctionContext {
    /// Function name
    pub name: Option<String>,
    /// Instructions
    pub instructions: Vec<Instruction>,
    /// Register allocator
    pub registers: RegisterAllocator,
    /// Scope chain
    pub scopes: ScopeChain,
    /// Function flags
    pub flags: FunctionFlags,
    /// Number of parameters
    pub param_count: u8,
    /// Captured upvalues from parent scopes
    pub upvalues: Vec<UpvalueCapture>,
    /// Number of Inline Cache slots
    pub ic_count: u16,
    /// Source offsets for each emitted instruction (parallel to `instructions`).
    pub source_offsets: Vec<u32>,
    /// Current source offset used for subsequent emitted instructions.
    pub current_source_offset: u32,
    /// Local variable index holding the 'arguments' object (if created).
    /// Stored as a local (not a register) so it survives inner function calls.
    pub arguments_local: Option<u16>,
}

impl FunctionContext {
    /// Create a new function context
    pub fn new(name: Option<String>) -> Self {
        let mut scopes = ScopeChain::new();
        scopes.enter(true); // Function scope

        Self {
            name,
            instructions: Vec::new(),
            registers: RegisterAllocator::new(),
            scopes,
            flags: FunctionFlags::default(),
            param_count: 0,
            upvalues: Vec::new(),
            ic_count: 0,
            source_offsets: Vec::new(),
            current_source_offset: 0,
            arguments_local: None,
        }
    }

    /// Emit an instruction
    pub fn emit(&mut self, instruction: Instruction) {
        self.instructions.push(instruction);
        self.source_offsets.push(self.current_source_offset);
    }

    /// Get current instruction index (for patching jumps)
    pub fn current_index(&self) -> usize {
        self.instructions.len()
    }

    /// Patch a jump instruction
    pub fn patch_jump(&mut self, index: usize, offset: i32) {
        match &mut self.instructions[index] {
            Instruction::Jump { offset: o } => *o = JumpOffset(offset),
            Instruction::JumpIfTrue { offset: o, .. } => *o = JumpOffset(offset),
            Instruction::JumpIfFalse { offset: o, .. } => *o = JumpOffset(offset),
            Instruction::JumpIfNullish { offset: o, .. } => *o = JumpOffset(offset),
            Instruction::JumpIfNotNullish { offset: o, .. } => *o = JumpOffset(offset),
            Instruction::TryStart { catch_offset: o } => *o = JumpOffset(offset),
            Instruction::ForInNext { offset: o, .. } => *o = JumpOffset(offset),
            _ => panic!("Not a jump instruction"),
        }
    }

    /// Build the function
    pub fn build(self, line_starts: &[u32]) -> Function {
        let mut source_map = SourceMap::new();
        for (instruction_index, source_offset) in self.source_offsets.iter().enumerate() {
            let (line, column) = offset_to_line_column(*source_offset, line_starts);
            source_map.add(instruction_index as u32, *source_offset, line, column);
        }

        FunctionBuilder::new()
            .name(self.name.unwrap_or_default())
            .param_count(self.param_count)
            .local_count(self.scopes.local_count())
            .local_names(self.scopes.collect_local_names())
            .register_count(self.registers.max_used())
            .flags(self.flags)
            .upvalues(self.upvalues)
            .instructions(self.instructions)
            .feedback_vector_size(self.ic_count as usize)
            .source_map(source_map)
            .build()
    }
}

fn offset_to_line_column(offset: u32, line_starts: &[u32]) -> (u32, u32) {
    if line_starts.is_empty() {
        return (1, offset.saturating_add(1));
    }

    let idx = line_starts.partition_point(|start| *start <= offset);
    let line_index = idx.saturating_sub(1);
    let line_start = line_starts[line_index];

    let line = (line_index as u32).saturating_add(1);
    let column = offset.saturating_sub(line_start).saturating_add(1);
    (line, column)
}

/// Code generator state
pub struct CodeGen {
    /// Constant pool
    pub constants: ConstantPool,
    /// Functions
    pub functions: Vec<Function>,
    /// Current function context
    pub current: FunctionContext,
    /// Function context stack (for nested functions)
    func_stack: Vec<FunctionContext>,
    /// Import records collected during compilation
    imports: Vec<ImportRecord>,
    /// Export records collected during compilation
    exports: Vec<ExportRecord>,
    /// Whether this is an ES module
    is_esm: bool,
    /// Whether to run peephole optimization
    optimize: bool,
    /// Start offsets for each source line (0-based byte offsets).
    line_starts: Vec<u32>,
}

impl CodeGen {
    /// Create a new code generator
    pub fn new() -> Self {
        Self {
            constants: ConstantPool::new(),
            functions: Vec::new(),
            current: FunctionContext::new(Some("main".to_string())),
            func_stack: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            is_esm: false,
            optimize: false,
            line_starts: vec![0],
        }
    }

    /// Create a new code generator with optimization enabled
    pub fn with_optimization(optimize: bool) -> Self {
        Self {
            constants: ConstantPool::new(),
            functions: Vec::new(),
            current: FunctionContext::new(Some("main".to_string())),
            func_stack: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            is_esm: false,
            optimize,
            line_starts: vec![0],
        }
    }

    /// Set source line start offsets for source-map generation.
    pub fn set_line_starts(&mut self, line_starts: Vec<u32>) {
        self.line_starts = line_starts;
    }

    /// Set current source offset for subsequent emitted instructions.
    pub fn set_current_source_offset(&mut self, source_offset: u32) {
        self.current.current_source_offset = source_offset;
    }

    /// Get current source offset used by the active function context.
    pub fn current_source_offset(&self) -> u32 {
        self.current.current_source_offset
    }

    /// Enable or disable optimization
    pub fn set_optimize(&mut self, optimize: bool) {
        self.optimize = optimize;
    }

    /// Add a string constant
    pub fn add_string(&mut self, s: &str) -> ConstantIndex {
        ConstantIndex(self.constants.add_string(s))
    }

    /// Add a UTF-16 string constant
    pub fn add_string_units(&mut self, units: Vec<u16>) -> ConstantIndex {
        ConstantIndex(self.constants.add_string_units(units))
    }

    /// Add a number constant
    pub fn add_number(&mut self, n: f64) -> ConstantIndex {
        ConstantIndex(self.constants.add_number(n))
    }

    /// Add a BigInt constant
    pub fn add_bigint(&mut self, s: String) -> ConstantIndex {
        ConstantIndex(self.constants.add(Constant::bigint(s)))
    }

    /// Add a RegExp constant
    pub fn add_regexp(&mut self, pattern: &str, flags: &str) -> ConstantIndex {
        ConstantIndex(self.constants.add(Constant::regexp(pattern, flags)))
    }

    /// Add a Symbol constant
    pub fn add_symbol(&mut self, id: u64) -> ConstantIndex {
        ConstantIndex(self.constants.add(Constant::Symbol(id)))
    }

    /// Add a tagged template literal site constant
    pub fn add_template_literal(
        &mut self,
        site_id: u32,
        cooked: Vec<Option<Vec<u16>>>,
        raw: Vec<Vec<u16>>,
    ) -> ConstantIndex {
        ConstantIndex(
            self.constants
                .add(Constant::template_literal(site_id, cooked, raw)),
        )
    }

    /// Emit an instruction
    pub fn emit(&mut self, instruction: Instruction) {
        self.current.emit(instruction);
    }

    /// Allocate an IC index
    pub fn alloc_ic(&mut self) -> u16 {
        let id = self.current.ic_count;
        self.current.ic_count += 1;
        id
    }

    /// Allocate a register
    pub fn alloc_reg(&mut self) -> Register {
        self.current.registers.alloc()
    }

    /// Allocate a contiguous block of new registers (does not use the free-list).
    pub fn alloc_fresh_block(&mut self, count: u8) -> Register {
        self.current.registers.alloc_fresh_block(count)
    }

    /// Free a register
    pub fn free_reg(&mut self, reg: Register) {
        self.current.registers.free(reg);
    }

    /// Get current instruction index
    pub fn current_index(&self) -> usize {
        self.current.current_index()
    }

    /// Patch a jump
    pub fn patch_jump(&mut self, index: usize, offset: i32) {
        self.current.patch_jump(index, offset);
    }

    /// Emit a placeholder jump (returns index for patching)
    pub fn emit_jump(&mut self) -> usize {
        let idx = self.current_index();
        self.emit(Instruction::Jump {
            offset: JumpOffset(0),
        });
        idx
    }

    /// Emit a conditional jump (returns index for patching)
    pub fn emit_jump_if_false(&mut self, cond: Register) -> usize {
        let idx = self.current_index();
        self.emit(Instruction::JumpIfFalse {
            cond,
            offset: JumpOffset(0),
        });
        idx
    }

    /// Emit a conditional jump if true (returns index for patching)
    pub fn emit_jump_if_true(&mut self, cond: Register) -> usize {
        let idx = self.current_index();
        self.emit(Instruction::JumpIfTrue {
            cond,
            offset: JumpOffset(0),
        });
        idx
    }

    /// Enter a block scope
    pub fn enter_scope(&mut self) {
        self.current.scopes.enter(false);
    }

    /// Exit a block scope
    pub fn exit_scope(&mut self) {
        self.current.scopes.exit();
    }

    /// Declare a variable with a specific kind
    pub fn declare_variable_with_kind(
        &mut self,
        name: &str,
        kind: VariableKind,
    ) -> CompileResult<u16> {
        self.current.scopes.declare(name, kind).ok_or_else(|| {
            CompileError::syntax(
                format!("Identifier '{}' has already been declared", name),
                0,
                0,
            )
        })
    }

    /// Declare an Annex B synthetic var-extension binding.
    /// Returns `Some((index, is_new))` — `is_new` is true if newly created.
    pub fn declare_block_function_var_extension(&mut self, name: &str) -> Option<(u16, bool)> {
        self.current
            .scopes
            .declare_block_function_var_extension(name)
    }

    /// Check if name is a CatchParameter in the enclosing scope chain.
    pub fn find_catch_parameter(&self, name: &str) -> Option<u16> {
        self.current.scopes.find_catch_parameter(name)
    }

    /// Declare a variable (is_const: true = Const, false = Let)
    /// For var declarations, use declare_variable_with_kind with VariableKind::Var
    pub fn declare_variable(&mut self, name: &str, is_const: bool) -> CompileResult<u16> {
        let kind = if is_const {
            VariableKind::Const
        } else {
            VariableKind::Let
        };
        self.declare_variable_with_kind(name, kind)
    }

    /// Resolve a variable
    /// First searches the current function's scope, then traverses parent functions
    pub fn resolve_variable(&self, name: &str) -> Option<ResolvedBinding> {
        // First try to resolve in current function's scope chain
        if let Some(binding) = self.current.scopes.resolve(name) {
            // Only return immediately for Local bindings
            // For Global, we need to check parent functions first
            match &binding {
                ResolvedBinding::Local(_) | ResolvedBinding::Upvalue { .. } => {
                    return Some(binding);
                }
                ResolvedBinding::Global(_) => {
                    // Don't return yet - check parent functions first
                }
            }
        }

        // Search parent function contexts
        // depth starts at 1 because we're looking at the first parent
        for (depth, parent_ctx) in self.func_stack.iter().rev().enumerate() {
            if let Some(binding) = parent_ctx.scopes.resolve(name) {
                match binding {
                    ResolvedBinding::Local(idx) => {
                        // Variable is in a parent function's local scope
                        return Some(ResolvedBinding::Upvalue {
                            index: idx,
                            depth: depth + 1, // +1 because we're looking at parents
                        });
                    }
                    ResolvedBinding::Upvalue {
                        index,
                        depth: inner_depth,
                    } => {
                        // Variable is already an upvalue in parent (transitive capture)
                        return Some(ResolvedBinding::Upvalue {
                            index,
                            depth: depth + 1 + inner_depth,
                        });
                    }
                    ResolvedBinding::Global(_) => {
                        // Continue searching other parents
                        continue;
                    }
                }
            }
        }

        // Not found in any parent - must be global
        Some(ResolvedBinding::Global(name.to_string()))
    }

    /// Resolve the `arguments` binding when regular lexical resolution falls back to global.
    ///
    /// For non-arrow functions, this lazily creates the local `arguments` object slot in the
    /// current function and emits initialization bytecode.
    ///
    /// For arrow functions, this finds the nearest non-arrow parent function, lazily creates
    /// its `arguments` slot (and initialization bytecode in that parent), then returns an
    /// upvalue binding from the current function to that slot.
    pub fn resolve_or_create_arguments_binding(&mut self) -> Option<ResolvedBinding> {
        if !self.current.flags.is_arrow {
            // Top-level "main" does not have an own `arguments` binding.
            if self.func_stack.is_empty() {
                return None;
            }

            let local_idx = if let Some(local_idx) = self.current.arguments_local {
                local_idx
            } else {
                let local_idx = self.current.scopes.alloc_anonymous_local()?;
                let tmp = self.alloc_reg();
                self.emit(Instruction::CreateArguments { dst: tmp });
                self.emit(Instruction::SetLocal {
                    idx: otter_vm_bytecode::LocalIndex(local_idx),
                    src: tmp,
                });
                self.free_reg(tmp);
                self.current.arguments_local = Some(local_idx);
                local_idx
            };
            return Some(ResolvedBinding::Local(local_idx));
        }

        for idx in (0..self.func_stack.len()).rev() {
            // Skip synthetic top-level context (`main`): arrows at module/script top-level
            // must not capture an implicit `arguments`.
            if idx == 0 {
                continue;
            }

            let parent_ctx = &mut self.func_stack[idx];
            if parent_ctx.flags.is_arrow {
                continue;
            }

            let local_idx = if let Some(local_idx) = parent_ctx.arguments_local {
                local_idx
            } else {
                let local_idx = parent_ctx.scopes.alloc_anonymous_local()?;
                let tmp = parent_ctx.registers.alloc();
                parent_ctx
                    .instructions
                    .push(Instruction::CreateArguments { dst: tmp });
                parent_ctx
                    .source_offsets
                    .push(parent_ctx.current_source_offset);
                parent_ctx.instructions.push(Instruction::SetLocal {
                    idx: otter_vm_bytecode::LocalIndex(local_idx),
                    src: tmp,
                });
                parent_ctx
                    .source_offsets
                    .push(parent_ctx.current_source_offset);
                parent_ctx.registers.free(tmp);
                parent_ctx.arguments_local = Some(local_idx);
                local_idx
            };

            return Some(ResolvedBinding::Upvalue {
                index: local_idx,
                depth: self.func_stack.len() - idx,
            });
        }

        None
    }

    /// Register an upvalue and return its index in the current function's upvalue array.
    ///
    /// This method handles both direct captures (depth=1) and transitive captures (depth>1).
    /// For transitive captures, it ensures all intermediate functions in the scope chain
    /// have proper upvalue captures.
    ///
    /// - `local_index`: The local variable index in the owning function
    /// - `depth`: How many function scopes up the variable is defined (1 = immediate parent)
    pub fn register_upvalue(&mut self, local_index: u16, depth: usize) -> u16 {
        if depth == 1 {
            // Direct capture from immediate parent's local variable
            let capture = UpvalueCapture::Local(otter_vm_bytecode::LocalIndex(local_index));
            self.add_upvalue_to_current(capture)
        } else {
            // Transitive capture: ensure all intermediate parents have upvalues,
            // then current captures from immediate parent's upvalue
            let parent_upvalue_idx = self.ensure_transitive_upvalues(local_index, depth);
            let capture =
                UpvalueCapture::Upvalue(otter_vm_bytecode::LocalIndex(parent_upvalue_idx));
            self.add_upvalue_to_current(capture)
        }
    }

    /// Add an upvalue capture to the current function, deduplicating if already exists.
    fn add_upvalue_to_current(&mut self, capture: UpvalueCapture) -> u16 {
        // Check if we already captured this exact upvalue
        for (i, existing) in self.current.upvalues.iter().enumerate() {
            if *existing == capture {
                return i as u16;
            }
        }

        // Add new upvalue capture
        let idx = self.current.upvalues.len() as u16;
        self.current.upvalues.push(capture);
        idx
    }

    /// Add an upvalue capture to a specific function in the stack, deduplicating if already exists.
    fn add_upvalue_to_func(&mut self, func_stack_idx: usize, capture: UpvalueCapture) -> u16 {
        let func = &mut self.func_stack[func_stack_idx];

        // Check if already captured
        for (i, existing) in func.upvalues.iter().enumerate() {
            if *existing == capture {
                return i as u16;
            }
        }

        // Add new upvalue
        let idx = func.upvalues.len() as u16;
        func.upvalues.push(capture);
        idx
    }

    /// Ensure all intermediate functions have upvalues for transitive capture.
    ///
    /// For a variable at depth D (where D > 1):
    /// - The variable is defined in func_stack[len - D]
    /// - Each function from func_stack[len - D + 1] to func_stack[len - 1] needs an upvalue
    /// - Returns the upvalue index in the immediate parent (func_stack[len - 1])
    ///
    /// Example: func_stack = [outer, middle, inner], current=innermost, depth=3
    /// - Variable is in outer (func_stack[0])
    /// - middle (func_stack[1]) needs Local upvalue for outer's local
    /// - inner (func_stack[2]) needs Upvalue for middle's upvalue
    /// - innermost (current) needs Upvalue for inner's upvalue
    fn ensure_transitive_upvalues(&mut self, local_index: u16, depth: usize) -> u16 {
        let stack_len = self.func_stack.len();

        // owner_idx = len - depth: the function that owns the local variable
        // first_capturer = owner_idx + 1: the first function that needs an upvalue
        let owner_idx = stack_len.saturating_sub(depth);
        let first_capturer = owner_idx + 1;

        // Safety check
        if first_capturer > stack_len {
            panic!(
                "Invalid depth {} for stack length {} (owner_idx={}, first_capturer={})",
                depth, stack_len, owner_idx, first_capturer
            );
        }

        // Add Local capture to first capturer (captures directly from owner's local)
        let local_capture = UpvalueCapture::Local(otter_vm_bytecode::LocalIndex(local_index));
        let mut prev_idx = self.add_upvalue_to_func(first_capturer, local_capture);

        // Add Upvalue captures to subsequent functions in the chain
        for func_idx in (first_capturer + 1)..stack_len {
            let upvalue_capture = UpvalueCapture::Upvalue(otter_vm_bytecode::LocalIndex(prev_idx));
            prev_idx = self.add_upvalue_to_func(func_idx, upvalue_capture);
        }

        // Return the upvalue index in the immediate parent (func_stack[len-1])
        prev_idx
    }

    /// Start compiling a new function
    pub fn enter_function(&mut self, name: Option<String>) {
        let old = std::mem::replace(&mut self.current, FunctionContext::new(name));
        self.func_stack.push(old);
    }

    /// Finish compiling current function
    pub fn exit_function(&mut self) -> u32 {
        let func = std::mem::replace(
            &mut self.current,
            self.func_stack.pop().expect("function stack underflow"),
        );
        let idx = self.functions.len() as u32;
        self.functions.push(func.build(&self.line_starts));
        idx
    }

    /// Add an import record
    pub fn add_import(&mut self, record: ImportRecord) {
        self.imports.push(record);
        self.is_esm = true;
    }

    /// Add an export record
    pub fn add_export(&mut self, record: ExportRecord) {
        self.exports.push(record);
        self.is_esm = true;
    }

    /// Set whether this is an ES module
    pub fn set_esm(&mut self, is_esm: bool) {
        self.is_esm = is_esm;
    }

    /// Get current imports
    pub fn imports(&self) -> &[ImportRecord] {
        &self.imports
    }

    /// Get current exports
    pub fn exports(&self) -> &[ExportRecord] {
        &self.exports
    }

    /// Check if this is an ES module
    pub fn is_esm(&self) -> bool {
        self.is_esm
    }

    /// Finalize compilation
    pub fn finish(mut self, source_url: &str) -> Module {
        // Add main function at the end (don't shift child function indices!)
        let main = self.current.build(&self.line_starts);
        let entry_point = self.functions.len() as u32;
        self.functions.push(main);

        // Run peephole optimization if enabled
        if self.optimize {
            let mut optimizer = PeepholeOptimizer::new();
            for func in &mut self.functions {
                optimizer.optimize(func.instructions.write());
            }
        }

        // Check if the entry function contains a top-level Await instruction
        let has_tla = self.functions[entry_point as usize]
            .instructions
            .read()
            .iter()
            .any(|instr| matches!(instr, Instruction::Await { .. }));

        let mut builder = Module::builder(source_url)
            .constants(self.constants)
            .entry_point(entry_point)
            .is_esm(self.is_esm)
            .has_top_level_await(has_tla);

        // Add imports
        for import in self.imports {
            builder = builder.import(import);
        }

        // Add exports
        for export in self.exports {
            builder = builder.export(export);
        }

        builder.build_with_functions(self.functions)
    }
}

impl Default for CodeGen {
    fn default() -> Self {
        Self::new()
    }
}

// Extension trait for ModuleBuilder
trait ModuleBuilderExt {
    fn build_with_functions(self, functions: Vec<Function>) -> Module;
}

impl ModuleBuilderExt for otter_vm_bytecode::module::ModuleBuilder {
    fn build_with_functions(mut self, functions: Vec<Function>) -> Module {
        for func in functions {
            self.add_function(func);
        }
        self.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_allocator() {
        let mut alloc = RegisterAllocator::new();

        let r0 = alloc.alloc();
        let r1 = alloc.alloc();
        let r2 = alloc.alloc();

        assert_eq!(r0.0, 0);
        assert_eq!(r1.0, 1);
        assert_eq!(r2.0, 2);
        assert_eq!(alloc.max_used(), 3);

        alloc.free(r2);
        let r3 = alloc.alloc();
        assert_eq!(r3.0, 2); // Reuses freed register
    }

    #[test]
    fn test_codegen_basic() {
        let mut cg = CodeGen::new();

        // Generate: return 42
        let dst = cg.alloc_reg();
        cg.emit(Instruction::LoadInt32 { dst, value: 42 });
        cg.emit(Instruction::Return { src: dst });

        let module = cg.finish("test.js");
        assert_eq!(module.functions.len(), 1);
    }
}
