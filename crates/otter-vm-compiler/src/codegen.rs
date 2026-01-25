//! Bytecode generation from AST

use otter_vm_bytecode::{
    ConstantIndex, ConstantPool, Function, Instruction, JumpOffset, Module, Register,
    UpvalueCapture,
    function::{FunctionBuilder, FunctionFlags},
    module::{ExportRecord, ImportRecord},
};

use crate::error::{CompileError, CompileResult};
use crate::scope::{ResolvedBinding, ScopeChain};
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
    /// Tracks which registers are currently allocated.
    in_use: Vec<bool>,
}

impl RegisterAllocator {
    /// Create a new register allocator
    pub fn new() -> Self {
        Self {
            next: 0,
            max: 0,
            free: Vec::new(),
            in_use: vec![false; 65536],
        }
    }

    /// Allocate a register
    pub fn alloc(&mut self) -> Register {
        if let Some(id) = self.free.pop() {
            debug_assert!(!self.in_use[id as usize], "register {id} already in use");
            self.in_use[id as usize] = true;
            Register(id)
        } else {
            let reg = Register(self.next);
            debug_assert!(
                !self.in_use[reg.0 as usize],
                "register {} already in use",
                reg.0
            );
            self.in_use[reg.0 as usize] = true;
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
        debug_assert!(
            self.in_use[reg.0 as usize],
            "freeing register {} that is not in use",
            reg.0
        );
        self.in_use[reg.0 as usize] = false;
        self.free.push(reg.0);
    }

    /// Get current position (for restoring later)
    pub fn position(&self) -> u16 {
        self.next
    }

    /// Restore to a previous position
    pub fn restore(&mut self, pos: u16) {
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
        }
    }

    /// Emit an instruction
    pub fn emit(&mut self, instruction: Instruction) {
        self.instructions.push(instruction);
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
    pub fn build(self) -> Function {
        FunctionBuilder::new()
            .name(self.name.unwrap_or_default())
            .param_count(self.param_count)
            .local_count(self.scopes.local_count())
            .register_count(self.registers.max_used())
            .flags(self.flags)
            .upvalues(self.upvalues)
            .instructions(self.instructions)
            .feedback_vector_size(self.ic_count as usize)
            .build()
    }
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
        }
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

    /// Declare a variable
    pub fn declare_variable(&mut self, name: &str, is_const: bool) -> CompileResult<u16> {
        self.current.scopes.declare(name, is_const).ok_or_else(|| {
            CompileError::syntax(
                format!("Identifier '{}' has already been declared", name),
                0,
                0,
            )
        })
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

    /// Register an upvalue and return its index in the current function's upvalue array.
    /// This method checks if the upvalue is already registered and returns the existing index,
    /// or adds a new upvalue capture and returns the new index.
    pub fn register_upvalue(&mut self, parent_local_index: u16, depth: usize) -> u16 {
        // For depth=1, capture directly from parent's local
        // For depth>1, we'd need transitive captures through parent upvalues
        // For now, we only support depth=1 (immediate parent)

        let capture = if depth == 1 {
            UpvalueCapture::Local(otter_vm_bytecode::LocalIndex(parent_local_index))
        } else {
            // For deeper captures, we need to register the upvalue in the parent first
            // and then capture from parent's upvalue. This is complex, so for now
            // we'll treat it as capturing from local at depth 1 (simplified).
            // TODO: Implement proper transitive upvalue capture
            UpvalueCapture::Local(otter_vm_bytecode::LocalIndex(parent_local_index))
        };

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
        self.functions.push(func.build());
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
        let main = self.current.build();
        let entry_point = self.functions.len() as u32;
        self.functions.push(main);

        let mut builder = Module::builder(source_url)
            .constants(self.constants)
            .entry_point(entry_point)
            .is_esm(self.is_esm);

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
