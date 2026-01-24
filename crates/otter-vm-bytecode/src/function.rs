//! Function bytecode representation

use serde::{Deserialize, Serialize};

use crate::instruction::Instruction;
use crate::operand::LocalIndex;

/// Function flags
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFlags {
    /// Is this an async function
    pub is_async: bool,
    /// Is this a generator function
    pub is_generator: bool,
    /// Is this an arrow function
    pub is_arrow: bool,
    /// Does this function use `arguments`
    pub uses_arguments: bool,
    /// Does this function use `eval`
    pub uses_eval: bool,
    /// Is strict mode
    pub is_strict: bool,
    /// Is a constructor
    pub is_constructor: bool,
    /// Is a method
    pub is_method: bool,
    /// Is a getter
    pub is_getter: bool,
    /// Is a setter
    pub is_setter: bool,
    /// Has rest parameter (...args)
    pub has_rest: bool,
}

/// Upvalue capture mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpvalueCapture {
    /// Capture from parent's local variable
    Local(LocalIndex),
    /// Capture from parent's upvalue (transitive capture)
    Upvalue(LocalIndex),
}

/// A bytecode function
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    /// Function name (empty for anonymous)
    pub name: Option<String>,

    /// Number of parameters (not including rest)
    pub param_count: u8,

    /// Number of local variables (including params)
    pub local_count: u16,

    /// Number of registers needed
    pub register_count: u8,

    /// Function flags
    pub flags: FunctionFlags,

    /// Upvalue captures
    pub upvalues: Vec<UpvalueCapture>,

    /// Bytecode instructions
    pub instructions: Vec<Instruction>,

    /// Source location mapping (instruction index -> source offset)
    pub source_map: Option<SourceMap>,

    /// Parameter names (for debugging)
    pub param_names: Vec<String>,

    /// Local variable names (for debugging)
    pub local_names: Vec<String>,
}

impl Function {
    /// Create a new function builder
    pub fn builder() -> FunctionBuilder {
        FunctionBuilder::new()
    }

    /// Get the function name or `<anonymous>`
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("<anonymous>")
    }

    /// Check if function is async
    #[inline]
    pub fn is_async(&self) -> bool {
        self.flags.is_async
    }

    /// Check if function is a generator
    #[inline]
    pub fn is_generator(&self) -> bool {
        self.flags.is_generator
    }

    /// Check if function is async generator
    #[inline]
    pub fn is_async_generator(&self) -> bool {
        self.flags.is_async && self.flags.is_generator
    }

    /// Check if function is an arrow function
    #[inline]
    pub fn is_arrow(&self) -> bool {
        self.flags.is_arrow
    }

    /// Check if function is in strict mode
    #[inline]
    pub fn is_strict(&self) -> bool {
        self.flags.is_strict
    }
}

/// Builder for creating functions
#[derive(Debug, Default)]
pub struct FunctionBuilder {
    name: Option<String>,
    param_count: u8,
    local_count: u16,
    register_count: u8,
    flags: FunctionFlags,
    upvalues: Vec<UpvalueCapture>,
    instructions: Vec<Instruction>,
    source_map: Option<SourceMap>,
    param_names: Vec<String>,
    local_names: Vec<String>,
}

impl FunctionBuilder {
    /// Create a new function builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Set function name
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set parameter count
    pub fn param_count(mut self, count: u8) -> Self {
        self.param_count = count;
        self
    }

    /// Set local variable count
    pub fn local_count(mut self, count: u16) -> Self {
        self.local_count = count;
        self
    }

    /// Set register count
    pub fn register_count(mut self, count: u8) -> Self {
        self.register_count = count;
        self
    }

    /// Set flags
    pub fn flags(mut self, flags: FunctionFlags) -> Self {
        self.flags = flags;
        self
    }

    /// Mark as async
    pub fn is_async(mut self, value: bool) -> Self {
        self.flags.is_async = value;
        self
    }

    /// Mark as generator
    pub fn is_generator(mut self, value: bool) -> Self {
        self.flags.is_generator = value;
        self
    }

    /// Mark as arrow function
    pub fn is_arrow(mut self, value: bool) -> Self {
        self.flags.is_arrow = value;
        self
    }

    /// Mark as strict mode
    pub fn is_strict(mut self, value: bool) -> Self {
        self.flags.is_strict = value;
        self
    }

    /// Add upvalue capture
    pub fn upvalue(mut self, capture: UpvalueCapture) -> Self {
        self.upvalues.push(capture);
        self
    }

    /// Set all upvalue captures
    pub fn upvalues(mut self, upvalues: Vec<UpvalueCapture>) -> Self {
        self.upvalues = upvalues;
        self
    }

    /// Set all instructions
    pub fn instructions(mut self, instructions: Vec<Instruction>) -> Self {
        self.instructions = instructions;
        self
    }

    /// Add a single instruction
    pub fn instruction(mut self, instruction: Instruction) -> Self {
        self.instructions.push(instruction);
        self
    }

    /// Set source map
    pub fn source_map(mut self, source_map: SourceMap) -> Self {
        self.source_map = Some(source_map);
        self
    }

    /// Add parameter name
    pub fn param_name(mut self, name: impl Into<String>) -> Self {
        self.param_names.push(name.into());
        self
    }

    /// Add local variable name
    pub fn local_name(mut self, name: impl Into<String>) -> Self {
        self.local_names.push(name.into());
        self
    }

    /// Build the function
    pub fn build(self) -> Function {
        Function {
            name: self.name,
            param_count: self.param_count,
            local_count: self.local_count,
            register_count: self.register_count,
            flags: self.flags,
            upvalues: self.upvalues,
            instructions: self.instructions,
            source_map: self.source_map,
            param_names: self.param_names,
            local_names: self.local_names,
        }
    }
}

/// Source location mapping
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceMap {
    /// Entries mapping instruction index to source location
    pub entries: Vec<SourceMapEntry>,
}

/// A single source map entry
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SourceMapEntry {
    /// Instruction index
    pub instruction_index: u32,
    /// Source file offset (bytes)
    pub source_offset: u32,
    /// Line number (1-indexed)
    pub line: u32,
    /// Column number (1-indexed)
    pub column: u32,
}

impl SourceMap {
    /// Create a new empty source map
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a mapping entry
    pub fn add(&mut self, instruction_index: u32, source_offset: u32, line: u32, column: u32) {
        self.entries.push(SourceMapEntry {
            instruction_index,
            source_offset,
            line,
            column,
        });
    }

    /// Find source location for instruction index
    pub fn find(&self, instruction_index: u32) -> Option<&SourceMapEntry> {
        // Binary search for the entry
        let idx = self
            .entries
            .binary_search_by_key(&instruction_index, |e| e.instruction_index);

        match idx {
            Ok(i) => Some(&self.entries[i]),
            Err(i) if i > 0 => Some(&self.entries[i - 1]),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operand::Register;

    #[test]
    fn test_function_builder() {
        let func = Function::builder()
            .name("add")
            .param_count(2)
            .local_count(2)
            .register_count(3)
            .is_strict(true)
            .instruction(Instruction::Add {
                dst: Register(0),
                lhs: Register(1),
                rhs: Register(2),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        assert_eq!(func.display_name(), "add");
        assert_eq!(func.param_count, 2);
        assert_eq!(func.instructions.len(), 2);
        assert!(func.is_strict());
    }

    #[test]
    fn test_source_map() {
        let mut map = SourceMap::new();
        map.add(0, 0, 1, 1);
        map.add(5, 20, 2, 5);
        map.add(10, 50, 3, 1);

        assert_eq!(map.find(0).unwrap().line, 1);
        assert_eq!(map.find(5).unwrap().line, 2);
        assert_eq!(map.find(7).unwrap().line, 2); // Between entries
        assert_eq!(map.find(10).unwrap().line, 3);
    }
}
