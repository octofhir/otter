//! Bytecode module format

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

use crate::constant::ConstantPool;
use crate::error::{BytecodeError, Result};
use crate::function::Function;
use crate::{BYTECODE_MAGIC, BYTECODE_VERSION};

/// Import record for a module
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRecord {
    /// Module specifier (e.g., "./utils.js" or "lodash")
    pub specifier: String,
    /// Imported bindings
    pub bindings: Vec<ImportBinding>,
}

/// A single import binding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImportBinding {
    /// import { foo } from "..."
    Named {
        /// Exported name
        imported: String,
        /// Local binding name
        local: String,
    },
    /// import * as foo from "..."
    Namespace {
        /// Local binding name
        local: String,
    },
    /// import foo from "..."
    Default {
        /// Local binding name
        local: String,
    },
}

/// Export record for a module
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExportRecord {
    /// export { foo }
    Named {
        /// Local name
        local: String,
        /// Exported name
        exported: String,
    },
    /// export default foo
    Default {
        /// Local name
        local: String,
    },
    /// export * from "..."
    ReExportAll {
        /// Source module specifier
        specifier: String,
    },
    /// export { foo } from "..."
    ReExportNamed {
        /// Source module specifier
        specifier: String,
        /// Imported name
        imported: String,
        /// Exported name
        exported: String,
    },
}

/// TypeScript type information (preserved from source)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeInfo {
    /// Type name or description
    pub name: String,
    /// Type kind
    pub kind: TypeKind,
}

/// Kind of TypeScript type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TypeKind {
    /// Primitive type (number, string, boolean, etc.)
    Primitive,
    /// Object type
    Object,
    /// Array type
    Array,
    /// Function type
    Function,
    /// Union type
    Union,
    /// Intersection type
    Intersection,
    /// Generic type
    Generic,
    /// Type alias
    Alias,
    /// Interface
    Interface,
    /// Enum
    Enum,
    /// Class type
    Class,
}

/// A compiled bytecode module
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Module {
    /// Source URL/path
    pub source_url: String,

    /// SHA256 hash of source for cache invalidation
    pub source_hash: [u8; 32],

    /// Constant pool (shared across all functions)
    pub constants: ConstantPool,

    /// Functions defined in this module
    pub functions: Vec<Function>,

    /// Entry point function index
    pub entry_point: u32,

    /// Import records
    pub imports: Vec<ImportRecord>,

    /// Export records
    pub exports: Vec<ExportRecord>,

    /// TypeScript type information
    pub types: Vec<TypeInfo>,

    /// Is this an ES module (vs CommonJS)
    pub is_esm: bool,

    /// Original source (optional, for debugging)
    pub source: Option<String>,
}

impl Module {
    /// Create a new module builder
    pub fn builder(source_url: impl Into<String>) -> ModuleBuilder {
        ModuleBuilder::new(source_url)
    }

    /// Serialize module to bytes
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();

        // Write magic
        bytes.extend_from_slice(&BYTECODE_MAGIC);

        // Write version
        bytes.extend_from_slice(&BYTECODE_VERSION.to_le_bytes());

        // Serialize module data with bincode
        let data = bincode_serialize(self)?;
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);

        Ok(bytes)
    }

    /// Deserialize module from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 16 {
            return Err(BytecodeError::UnexpectedEnd);
        }

        // Check magic
        if bytes[0..8] != BYTECODE_MAGIC {
            return Err(BytecodeError::InvalidMagic);
        }

        // Check version
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != BYTECODE_VERSION {
            return Err(BytecodeError::UnsupportedVersion(version));
        }

        // Read data length
        let data_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;

        if bytes.len() < 16 + data_len {
            return Err(BytecodeError::UnexpectedEnd);
        }

        // Deserialize module
        bincode_deserialize(&bytes[16..16 + data_len])
    }

    /// Write module to a writer
    pub fn write_to<W: Write>(&self, writer: &mut W) -> Result<()> {
        let bytes = self.to_bytes()?;
        writer.write_all(&bytes)?;
        Ok(())
    }

    /// Read module from a reader
    pub fn read_from<R: Read>(reader: &mut R) -> Result<Self> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Self::from_bytes(&bytes)
    }

    /// Get the entry point function
    pub fn entry_function(&self) -> Option<&Function> {
        self.functions.get(self.entry_point as usize)
    }

    /// Get a function by index
    pub fn function(&self, index: u32) -> Option<&Function> {
        self.functions.get(index as usize)
    }
}

/// Builder for creating modules
#[derive(Debug)]
pub struct ModuleBuilder {
    source_url: String,
    source_hash: [u8; 32],
    constants: ConstantPool,
    functions: Vec<Function>,
    entry_point: u32,
    imports: Vec<ImportRecord>,
    exports: Vec<ExportRecord>,
    types: Vec<TypeInfo>,
    is_esm: bool,
    source: Option<String>,
}

impl ModuleBuilder {
    /// Create a new module builder
    pub fn new(source_url: impl Into<String>) -> Self {
        Self {
            source_url: source_url.into(),
            source_hash: [0; 32],
            constants: ConstantPool::new(),
            functions: Vec::new(),
            entry_point: 0,
            imports: Vec::new(),
            exports: Vec::new(),
            types: Vec::new(),
            is_esm: true,
            source: None,
        }
    }

    /// Set source hash
    pub fn source_hash(mut self, hash: [u8; 32]) -> Self {
        self.source_hash = hash;
        self
    }

    /// Set constant pool
    pub fn constants(mut self, constants: ConstantPool) -> Self {
        self.constants = constants;
        self
    }

    /// Get mutable reference to constant pool
    pub fn constants_mut(&mut self) -> &mut ConstantPool {
        &mut self.constants
    }

    /// Add a function, returns its index
    pub fn add_function(&mut self, function: Function) -> u32 {
        let idx = self.functions.len() as u32;
        self.functions.push(function);
        idx
    }

    /// Set entry point function index
    pub fn entry_point(mut self, index: u32) -> Self {
        self.entry_point = index;
        self
    }

    /// Add an import record
    pub fn import(mut self, import: ImportRecord) -> Self {
        self.imports.push(import);
        self
    }

    /// Add an export record
    pub fn export(mut self, export: ExportRecord) -> Self {
        self.exports.push(export);
        self
    }

    /// Add type information
    pub fn type_info(mut self, info: TypeInfo) -> Self {
        self.types.push(info);
        self
    }

    /// Set ESM flag
    pub fn is_esm(mut self, value: bool) -> Self {
        self.is_esm = value;
        self
    }

    /// Include source code (for debugging)
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Build the module
    pub fn build(self) -> Module {
        Module {
            source_url: self.source_url,
            source_hash: self.source_hash,
            constants: self.constants,
            functions: self.functions,
            entry_point: self.entry_point,
            imports: self.imports,
            exports: self.exports,
            types: self.types,
            is_esm: self.is_esm,
            source: self.source,
        }
    }
}

// Helper functions for serialization using serde_json (bincode would be better but adds dependency)
fn bincode_serialize<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| {
        BytecodeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    })
}

fn bincode_deserialize<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| {
        BytecodeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruction::Instruction;
    use crate::operand::Register;

    #[test]
    fn test_module_roundtrip() {
        let mut builder = Module::builder("test.js");

        // Add some constants
        builder.constants_mut().add_string("hello");
        builder.constants_mut().add_number(42.0);

        // Add a simple function
        let func = Function::builder()
            .name("main")
            .instruction(Instruction::LoadTrue { dst: Register(0) })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        builder.add_function(func);

        let module = builder.build();

        // Serialize and deserialize
        let bytes = module.to_bytes().unwrap();
        let restored = Module::from_bytes(&bytes).unwrap();

        assert_eq!(restored.source_url, "test.js");
        assert_eq!(restored.constants.len(), 2);
        assert_eq!(restored.functions.len(), 1);
    }

    #[test]
    fn test_invalid_magic() {
        // Need at least 16 bytes to pass length check before magic check
        let bytes = b"INVALID\0........";
        let result = Module::from_bytes(bytes);
        assert!(matches!(result, Err(BytecodeError::InvalidMagic)));
    }

    #[test]
    fn test_import_binding() {
        let import = ImportRecord {
            specifier: "./utils.js".to_string(),
            bindings: vec![
                ImportBinding::Named {
                    imported: "foo".to_string(),
                    local: "foo".to_string(),
                },
                ImportBinding::Default {
                    local: "utils".to_string(),
                },
            ],
        };

        assert_eq!(import.specifier, "./utils.js");
        assert_eq!(import.bindings.len(), 2);
    }
}
