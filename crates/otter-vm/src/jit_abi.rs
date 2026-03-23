//! JIT-facing ABI placeholders for the new VM.

use crate::abi::VmAbiVersion;
use crate::bridge::{FunctionBoundary, RuntimeBoundary, function_boundary, runtime_boundary};
use crate::deopt::DeoptHandoff;
use crate::module::{FunctionIndex, Module};

/// Reports the ABI version expected by the future JIT integration.
#[must_use]
pub const fn jit_abi_version() -> VmAbiVersion {
    VmAbiVersion::V1
}

/// Stable key for one compiled JIT entrypoint under the shared VM ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JitEntrypointKey {
    abi_version: VmAbiVersion,
    function_index: FunctionIndex,
}

impl JitEntrypointKey {
    /// Creates a JIT entrypoint key.
    #[must_use]
    pub const fn new(function_index: FunctionIndex) -> Self {
        Self {
            abi_version: VmAbiVersion::V1,
            function_index,
        }
    }

    /// Returns the ABI version required by this entrypoint.
    #[must_use]
    pub const fn abi_version(self) -> VmAbiVersion {
        self.abi_version
    }

    /// Returns the function index.
    #[must_use]
    pub const fn function_index(self) -> FunctionIndex {
        self.function_index
    }
}

/// Compile-time request passed from `otter-vm` into the future JIT layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JitCompileRequest {
    runtime: RuntimeBoundary,
    function: FunctionBoundary,
}

impl JitCompileRequest {
    /// Creates a compile request for one function boundary.
    #[must_use]
    pub const fn new(runtime: RuntimeBoundary, function: FunctionBoundary) -> Self {
        Self { runtime, function }
    }

    /// Returns the shared runtime boundary.
    #[must_use]
    pub const fn runtime(self) -> RuntimeBoundary {
        self.runtime
    }

    /// Returns the function boundary to compile against.
    #[must_use]
    pub const fn function(self) -> FunctionBoundary {
        self.function
    }

    /// Returns the stable JIT entrypoint key.
    #[must_use]
    pub const fn entrypoint(self) -> JitEntrypointKey {
        JitEntrypointKey::new(self.function.function_index())
    }
}

/// Deopt handoff placeholder owned by a compiled JIT entrypoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JitDeoptPlaceholder {
    entrypoint: JitEntrypointKey,
    handoff: DeoptHandoff,
}

impl JitDeoptPlaceholder {
    /// Creates a deopt placeholder for one compiled entrypoint.
    #[must_use]
    pub const fn new(entrypoint: JitEntrypointKey, handoff: DeoptHandoff) -> Self {
        Self {
            entrypoint,
            handoff,
        }
    }

    /// Returns the owning JIT entrypoint key.
    #[must_use]
    pub const fn entrypoint(self) -> JitEntrypointKey {
        self.entrypoint
    }

    /// Returns the shared deopt handoff placeholder.
    #[must_use]
    pub const fn handoff(self) -> DeoptHandoff {
        self.handoff
    }
}

/// Builds a compile request for one function, if the function exists in the module.
#[must_use]
pub fn compile_request(
    module: &Module,
    function_index: FunctionIndex,
) -> Option<JitCompileRequest> {
    function_boundary(module, function_index)
        .map(|function| JitCompileRequest::new(runtime_boundary(), function))
}

#[cfg(test)]
mod tests {
    use crate::bytecode::{Bytecode, BytecodeRegister, Instruction};
    use crate::call::CallTable;
    use crate::closure::ClosureTable;
    use crate::deopt::{DeoptHandoff, DeoptId, DeoptReason, DeoptSite, DeoptTable};
    use crate::exception::ExceptionTable;
    use crate::feedback::FeedbackTableLayout;
    use crate::frame::FrameLayout;
    use crate::module::{Function, FunctionIndex, FunctionSideTables, FunctionTables, Module};
    use crate::property::PropertyNameTable;
    use crate::source_map::SourceMap;
    use crate::string::StringTable;

    use super::{JitDeoptPlaceholder, JitEntrypointKey, compile_request, jit_abi_version};

    #[test]
    fn compile_request_uses_shared_runtime_and_function_boundary() {
        let function = Function::new(
            Some("entry"),
            FrameLayout::new(0, 1, 0, 0).expect("layout should be valid"),
            Bytecode::from(vec![Instruction::ret(BytecodeRegister::new(0))]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                ),
                FeedbackTableLayout::default(),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let request = compile_request(&module, FunctionIndex(0)).expect("function should exist");

        assert_eq!(
            request.entrypoint(),
            JitEntrypointKey::new(FunctionIndex(0))
        );
        assert_eq!(request.runtime().abi().version(), jit_abi_version());
        assert_eq!(request.function().frame_abi().layout().parameter_count(), 1);
    }

    #[test]
    fn jit_deopt_placeholder_wraps_shared_handoff() {
        let site = DeoptSite::new(DeoptId(9), 3);
        let handoff = DeoptHandoff::at_site(site, DeoptReason::UnsupportedPath);
        let placeholder =
            JitDeoptPlaceholder::new(JitEntrypointKey::new(FunctionIndex(1)), handoff);

        assert_eq!(placeholder.entrypoint().function_index(), FunctionIndex(1));
        assert_eq!(placeholder.handoff(), handoff);
    }
}
