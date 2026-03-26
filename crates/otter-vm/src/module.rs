//! Executable module and function containers for the new VM.

use core::fmt;

use crate::bytecode::Bytecode;
use crate::call::CallTable;
use crate::closure::ClosureTable;
use crate::deopt::DeoptTable;
use crate::exception::ExceptionTable;
use crate::feedback::FeedbackTableLayout;
use crate::float::FloatTable;
use crate::frame::FrameLayout;
use crate::property::PropertyNameTable;
use crate::source_map::SourceMap;
use crate::string::StringTable;

/// Stable function index inside a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionIndex(pub u32);

/// Errors produced while constructing an executable module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleError {
    /// The module does not contain an entry function at the requested index.
    InvalidEntryFunction,
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEntryFunction => {
                f.write_str("module entry function index is out of bounds")
            }
        }
    }
}

impl std::error::Error for ModuleError {}

/// Immutable executable function for the new VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionSideTables {
    property_names: PropertyNameTable,
    string_literals: StringTable,
    float_constants: FloatTable,
    closures: ClosureTable,
    calls: CallTable,
}

impl FunctionSideTables {
    /// Creates the side-table group attached to a function.
    #[must_use]
    pub fn new(
        property_names: PropertyNameTable,
        string_literals: StringTable,
        float_constants: FloatTable,
        closures: ClosureTable,
        calls: CallTable,
    ) -> Self {
        Self {
            property_names,
            string_literals,
            float_constants,
            closures,
            calls,
        }
    }
}

impl Default for FunctionSideTables {
    fn default() -> Self {
        Self::new(
            PropertyNameTable::default(),
            StringTable::default(),
            FloatTable::default(),
            ClosureTable::default(),
            CallTable::default(),
        )
    }
}

/// Immutable executable function for the new VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionTables {
    side_tables: FunctionSideTables,
    feedback: FeedbackTableLayout,
    deopt: DeoptTable,
    exceptions: ExceptionTable,
    source_map: SourceMap,
}

impl FunctionTables {
    /// Creates an explicit side-table bundle for a function.
    #[must_use]
    pub fn new(
        side_tables: FunctionSideTables,
        feedback: FeedbackTableLayout,
        deopt: DeoptTable,
        exceptions: ExceptionTable,
        source_map: SourceMap,
    ) -> Self {
        Self {
            side_tables,
            feedback,
            deopt,
            exceptions,
            source_map,
        }
    }

    /// Creates an empty side-table bundle.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(
            FunctionSideTables::default(),
            FeedbackTableLayout::default(),
            DeoptTable::default(),
            ExceptionTable::default(),
            SourceMap::default(),
        )
    }
}

impl Default for FunctionTables {
    fn default() -> Self {
        Self::empty()
    }
}

/// Immutable executable function for the new VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    name: Option<Box<str>>,
    frame_layout: FrameLayout,
    bytecode: Bytecode,
    property_names: PropertyNameTable,
    string_literals: StringTable,
    float_constants: FloatTable,
    closures: ClosureTable,
    calls: CallTable,
    feedback: FeedbackTableLayout,
    deopt: DeoptTable,
    exceptions: ExceptionTable,
    source_map: SourceMap,
}

impl Function {
    /// Creates an executable function with explicit side tables.
    #[must_use]
    pub fn new(
        name: Option<impl Into<Box<str>>>,
        frame_layout: FrameLayout,
        bytecode: Bytecode,
        tables: FunctionTables,
    ) -> Self {
        Self {
            name: name.map(Into::into),
            frame_layout,
            bytecode,
            property_names: tables.side_tables.property_names,
            string_literals: tables.side_tables.string_literals,
            float_constants: tables.side_tables.float_constants,
            closures: tables.side_tables.closures,
            calls: tables.side_tables.calls,
            feedback: tables.feedback,
            deopt: tables.deopt,
            exceptions: tables.exceptions,
            source_map: tables.source_map,
        }
    }

    /// Creates a function with empty side tables.
    #[must_use]
    pub fn with_bytecode(
        name: Option<impl Into<Box<str>>>,
        frame_layout: FrameLayout,
        bytecode: Bytecode,
    ) -> Self {
        Self::new(name, frame_layout, bytecode, FunctionTables::default())
    }

    /// Returns the function name, if present.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns the frame layout.
    #[must_use]
    pub const fn frame_layout(&self) -> FrameLayout {
        self.frame_layout
    }

    /// Returns the immutable bytecode stream.
    #[must_use]
    pub fn bytecode(&self) -> &Bytecode {
        &self.bytecode
    }

    /// Returns the feedback side-table layout.
    #[must_use]
    pub fn feedback(&self) -> &FeedbackTableLayout {
        &self.feedback
    }

    /// Returns the property-name side table.
    #[must_use]
    pub fn property_names(&self) -> &PropertyNameTable {
        &self.property_names
    }

    /// Returns the string-literal side table.
    #[must_use]
    pub fn string_literals(&self) -> &StringTable {
        &self.string_literals
    }

    /// Returns the float-constant side table.
    #[must_use]
    pub fn float_constants(&self) -> &FloatTable {
        &self.float_constants
    }

    /// Returns the closure-creation side table.
    #[must_use]
    pub fn closures(&self) -> &ClosureTable {
        &self.closures
    }

    /// Returns the call-site side table.
    #[must_use]
    pub fn calls(&self) -> &CallTable {
        &self.calls
    }

    /// Returns the deoptimization table.
    #[must_use]
    pub fn deopt(&self) -> &DeoptTable {
        &self.deopt
    }

    /// Returns the exception table.
    #[must_use]
    pub fn exceptions(&self) -> &ExceptionTable {
        &self.exceptions
    }

    /// Returns the source map.
    #[must_use]
    pub fn source_map(&self) -> &SourceMap {
        &self.source_map
    }
}

/// Immutable executable module for the new VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    name: Option<Box<str>>,
    functions: Box<[Function]>,
    entry: FunctionIndex,
}

impl Module {
    /// Creates a checked executable module.
    pub fn new(
        name: Option<impl Into<Box<str>>>,
        functions: Vec<Function>,
        entry: FunctionIndex,
    ) -> Result<Self, ModuleError> {
        let functions = functions.into_boxed_slice();
        if usize::try_from(entry.0)
            .ok()
            .and_then(|index| functions.get(index))
            .is_none()
        {
            return Err(ModuleError::InvalidEntryFunction);
        }

        Ok(Self {
            name: name.map(Into::into),
            functions,
            entry,
        })
    }

    /// Returns the module name, if present.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns the number of functions in the module.
    #[must_use]
    pub fn function_count(&self) -> usize {
        self.functions.len()
    }

    /// Returns the entry function index.
    #[must_use]
    pub const fn entry(&self) -> FunctionIndex {
        self.entry
    }

    /// Returns the entry function.
    #[must_use]
    pub fn entry_function(&self) -> &Function {
        let index =
            usize::try_from(self.entry.0).expect("entry function index must fit into usize");
        &self.functions[index]
    }

    /// Returns the immutable function slice.
    #[must_use]
    pub fn functions(&self) -> &[Function] {
        &self.functions
    }

    /// Returns the function at the given index.
    #[must_use]
    pub fn function(&self, index: FunctionIndex) -> Option<&Function> {
        usize::try_from(index.0)
            .ok()
            .and_then(|position| self.functions.get(position))
    }
}

#[cfg(test)]
mod tests {
    use crate::bytecode::{Bytecode, BytecodeRegister, Instruction};
    use crate::deopt::{DeoptId, DeoptSite, DeoptTable};
    use crate::exception::{ExceptionHandler, ExceptionTable};
    use crate::feedback::{FeedbackKind, FeedbackSlotId, FeedbackSlotLayout, FeedbackTableLayout};
    use crate::float::FloatTable;
    use crate::frame::FrameLayout;
    use crate::property::PropertyNameTable;
    use crate::source_map::{SourceLocation, SourceMap, SourceMapEntry};
    use crate::string::StringTable;
    use crate::{
        call::{CallSite, CallTable},
        closure::ClosureTable,
        frame::FrameFlags,
    };

    use super::{Function, FunctionIndex, FunctionSideTables, FunctionTables, Module, ModuleError};

    #[test]
    fn function_keeps_bytecode_and_side_tables() {
        let frame_layout = FrameLayout::new(1, 0, 0, 1).expect("frame layout should be valid");
        let bytecode = Bytecode::from(vec![Instruction::ret(BytecodeRegister::new(0))]);
        let feedback = FeedbackTableLayout::new(vec![FeedbackSlotLayout::new(
            FeedbackSlotId(0),
            FeedbackKind::Call,
        )]);
        let deopt = DeoptTable::new(vec![DeoptSite::new(DeoptId(7), 1)]);
        let exceptions = ExceptionTable::new(vec![ExceptionHandler::new(1, 3, 5)]);
        let source_map = SourceMap::new(vec![SourceMapEntry::new(0, SourceLocation::new(4, 2))]);
        let property_names = PropertyNameTable::new(vec!["count"]);
        let string_literals = StringTable::new(vec!["otter"]);
        let calls = CallTable::new(vec![Some(CallSite::Direct(crate::call::DirectCall::new(
            FunctionIndex(0),
            1,
            FrameFlags::empty(),
        )))]);

        let function = Function::new(
            Some("main"),
            frame_layout,
            bytecode,
            FunctionTables::new(
                FunctionSideTables::new(
                    property_names,
                    string_literals,
                    FloatTable::default(),
                    ClosureTable::default(),
                    calls,
                ),
                feedback,
                deopt,
                exceptions,
                source_map,
            ),
        );

        assert_eq!(function.name(), Some("main"));
        assert_eq!(function.bytecode().len(), 1);
        assert_eq!(
            function
                .property_names()
                .get(crate::property::PropertyNameId(0)),
            Some("count")
        );
        assert_eq!(
            function.string_literals().get(crate::string::StringId(0)),
            Some("otter")
        );
        assert_eq!(
            function.calls().get_direct(0),
            Some(crate::call::DirectCall::new(
                FunctionIndex(0),
                1,
                FrameFlags::empty()
            ))
        );
        assert_eq!(function.feedback().len(), 1);
        assert_eq!(function.deopt().len(), 1);
        assert_eq!(function.exceptions().len(), 1);
        assert_eq!(function.source_map().len(), 1);
    }

    #[test]
    fn module_requires_valid_entry_function() {
        let frame_layout = FrameLayout::default();
        let function = Function::with_bytecode(Some("entry"), frame_layout, Bytecode::default());
        let result = Module::new(Some("m"), vec![function], FunctionIndex(1));

        assert_eq!(result, Err(ModuleError::InvalidEntryFunction));
    }

    #[test]
    fn module_exposes_entry_function_and_count() {
        let frame_layout = FrameLayout::default();
        let entry = Function::with_bytecode(Some("entry"), frame_layout, Bytecode::default());
        let helper = Function::with_bytecode(Some("helper"), frame_layout, Bytecode::default());
        let module = Module::new(Some("m"), vec![entry, helper], FunctionIndex(0))
            .expect("module should be valid");

        assert_eq!(module.name(), Some("m"));
        assert_eq!(module.function_count(), 2);
        assert_eq!(module.entry_function().name(), Some("entry"));
        assert_eq!(
            module.function(FunctionIndex(1)).and_then(Function::name),
            Some("helper")
        );
    }
}
