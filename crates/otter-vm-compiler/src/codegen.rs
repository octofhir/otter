//! Bytecode generation from AST

use otter_vm_bytecode::{
    function::{FunctionBuilder, FunctionFlags},
    ConstantIndex, ConstantPool, Function, Instruction, JumpOffset, Module, Register,
};

use crate::error::{CompileError, CompileResult};
use crate::scope::{ResolvedBinding, ScopeChain};

/// Register allocator
#[derive(Debug)]
pub struct RegisterAllocator {
    /// Next available register
    next: u8,
    /// Maximum register used
    max: u8,
}

impl RegisterAllocator {
    /// Create a new register allocator
    pub fn new() -> Self {
        Self { next: 0, max: 0 }
    }

    /// Allocate a register
    pub fn alloc(&mut self) -> Register {
        let reg = Register(self.next);
        self.next += 1;
        self.max = self.max.max(self.next);
        reg
    }

    /// Free a register
    pub fn free(&mut self, _reg: Register) {
        if self.next > 0 {
            self.next -= 1;
        }
    }

    /// Get current position (for restoring later)
    pub fn position(&self) -> u8 {
        self.next
    }

    /// Restore to a previous position
    pub fn restore(&mut self, pos: u8) {
        self.next = pos;
    }

    /// Get maximum registers used
    pub fn max_used(&self) -> u8 {
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
            .instructions(self.instructions)
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
}

impl CodeGen {
    /// Create a new code generator
    pub fn new() -> Self {
        Self {
            constants: ConstantPool::new(),
            functions: Vec::new(),
            current: FunctionContext::new(Some("main".to_string())),
            func_stack: Vec::new(),
        }
    }

    /// Add a string constant
    pub fn add_string(&mut self, s: &str) -> ConstantIndex {
        ConstantIndex(self.constants.add_string(s))
    }

    /// Add a number constant
    pub fn add_number(&mut self, n: f64) -> ConstantIndex {
        ConstantIndex(self.constants.add_number(n))
    }

    /// Emit an instruction
    pub fn emit(&mut self, instruction: Instruction) {
        self.current.emit(instruction);
    }

    /// Allocate a register
    pub fn alloc_reg(&mut self) -> Register {
        self.current.registers.alloc()
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
    pub fn resolve_variable(&self, name: &str) -> Option<ResolvedBinding> {
        self.current.scopes.resolve(name)
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

    /// Finalize compilation
    pub fn finish(mut self, source_url: &str) -> Module {
        // Add main function
        let main = self.current.build();
        self.functions.insert(0, main);

        Module::builder(source_url)
            .constants(self.constants)
            .entry_point(0)
            .build_with_functions(self.functions)
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
