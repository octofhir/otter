//! Integration boundary between `otter-vm` and the outer engine/runtime.

use crate::abi::{FrameAbiRequirements, RuntimeAbiRequirements};
use crate::module::{Function, FunctionIndex, Module};

/// Stable identifier used by the engine to select the new VM backend.
pub const BACKEND_NAME: &str = "otter-vm";

/// Shared runtime boundary exposed to the engine and future JIT integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeBoundary {
    backend_name: &'static str,
    abi: RuntimeAbiRequirements,
}

impl RuntimeBoundary {
    /// Returns the runtime boundary for the current VM backend.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            backend_name: BACKEND_NAME,
            abi: RuntimeAbiRequirements::current(),
        }
    }

    /// Returns the backend name.
    #[must_use]
    pub const fn backend_name(self) -> &'static str {
        self.backend_name
    }

    /// Returns the shared runtime ABI contract.
    #[must_use]
    pub const fn abi(self) -> RuntimeAbiRequirements {
        self.abi
    }
}

/// Per-function JIT/interpreter boundary derived from immutable module metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionBoundary {
    function_index: FunctionIndex,
    frame_abi: FrameAbiRequirements,
    feedback_slots: usize,
    call_sites: usize,
    closure_sites: usize,
    deopt_sites: usize,
    exception_handlers: usize,
    source_map_entries: usize,
}

impl FunctionBoundary {
    /// Builds a function boundary from immutable function metadata.
    #[must_use]
    pub fn new(function_index: FunctionIndex, function: &Function) -> Self {
        Self {
            function_index,
            frame_abi: FrameAbiRequirements::new(function.frame_layout()),
            feedback_slots: function.feedback().len(),
            call_sites: function.calls().len(),
            closure_sites: function.closures().len(),
            deopt_sites: function.deopt().len(),
            exception_handlers: function.exceptions().len(),
            source_map_entries: function.source_map().len(),
        }
    }

    /// Returns the function index inside the module.
    #[must_use]
    pub const fn function_index(self) -> FunctionIndex {
        self.function_index
    }

    /// Returns the shared frame ABI for the function.
    #[must_use]
    pub const fn frame_abi(self) -> FrameAbiRequirements {
        self.frame_abi
    }

    /// Returns the feedback slot count.
    #[must_use]
    pub const fn feedback_slots(self) -> usize {
        self.feedback_slots
    }

    /// Returns the call-site count.
    #[must_use]
    pub const fn call_sites(self) -> usize {
        self.call_sites
    }

    /// Returns the closure-site count.
    #[must_use]
    pub const fn closure_sites(self) -> usize {
        self.closure_sites
    }

    /// Returns the deopt site count.
    #[must_use]
    pub const fn deopt_sites(self) -> usize {
        self.deopt_sites
    }

    /// Returns the exception handler count.
    #[must_use]
    pub const fn exception_handlers(self) -> usize {
        self.exception_handlers
    }

    /// Returns the source-map entry count.
    #[must_use]
    pub const fn source_map_entries(self) -> usize {
        self.source_map_entries
    }
}

/// Returns the current runtime boundary.
#[must_use]
pub const fn runtime_boundary() -> RuntimeBoundary {
    RuntimeBoundary::current()
}

/// Returns the immutable boundary for one function, if it exists.
#[must_use]
pub fn function_boundary(module: &Module, index: FunctionIndex) -> Option<FunctionBoundary> {
    module
        .function(index)
        .map(|function| FunctionBoundary::new(index, function))
}

/// Returns the immutable boundary for the module entry function.
#[must_use]
pub fn entry_boundary(module: &Module) -> FunctionBoundary {
    FunctionBoundary::new(module.entry(), module.entry_function())
}

#[cfg(test)]
mod tests {
    use crate::bytecode::{Bytecode, BytecodeRegister, Instruction};
    use crate::call::CallTable;
    use crate::closure::ClosureTable;
    use crate::deopt::{DeoptId, DeoptSite, DeoptTable};
    use crate::exception::{ExceptionHandler, ExceptionTable};
    use crate::feedback::{FeedbackKind, FeedbackSlotId, FeedbackSlotLayout, FeedbackTableLayout};
    use crate::bigint::BigIntTable;
    use crate::float::FloatTable;
    use crate::frame::FrameLayout;
    use crate::module::{Function, FunctionIndex, FunctionSideTables, FunctionTables, Module};
    use crate::property::PropertyNameTable;
    use crate::source_map::{SourceLocation, SourceMap, SourceMapEntry};
    use crate::string::StringTable;

    use super::{BACKEND_NAME, entry_boundary, function_boundary, runtime_boundary};

    #[test]
    fn runtime_boundary_reports_backend_and_shared_abi() {
        let boundary = runtime_boundary();

        assert_eq!(boundary.backend_name(), BACKEND_NAME);
        assert!(boundary.abi().shared_frame_model());
        assert!(boundary.abi().shared_calling_convention());
    }

    #[test]
    fn function_boundary_summarizes_shared_contract_inputs() {
        let function = Function::new(
            Some("entry"),
            FrameLayout::new(0, 2, 1, 3).expect("layout should be valid"),
            Bytecode::from(vec![Instruction::ret(BytecodeRegister::new(0))]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["count"]),
                    StringTable::new(vec!["otter"]),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![FeedbackSlotLayout::new(
                    FeedbackSlotId(0),
                    FeedbackKind::Call,
                )]),
                DeoptTable::new(vec![DeoptSite::new(DeoptId(2), 0)]),
                ExceptionTable::new(vec![ExceptionHandler::new(0, 1, 0)]),
                SourceMap::new(vec![SourceMapEntry::new(0, SourceLocation::new(1, 1))]),
            ),
        );
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let boundary = function_boundary(&module, FunctionIndex(0)).expect("function should exist");
        let entry = entry_boundary(&module);

        assert_eq!(boundary.function_index(), FunctionIndex(0));
        assert_eq!(boundary.frame_abi().layout().parameter_count(), 2);
        assert_eq!(boundary.feedback_slots(), 1);
        assert_eq!(boundary.deopt_sites(), 1);
        assert_eq!(boundary.exception_handlers(), 1);
        assert_eq!(boundary.source_map_entries(), 1);
        assert_eq!(entry, boundary);
    }
}
