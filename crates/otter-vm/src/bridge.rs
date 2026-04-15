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
