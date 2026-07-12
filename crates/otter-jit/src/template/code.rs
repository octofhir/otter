//! Finalized template code objects and VM entry publication.
//!
//! # Contents
//! - [`TemplateCode`] — ownership of one finalized template compilation.
//! - The [`JitFunctionCode`] implementation entering through the shared
//!   compiled-entry ABI.
//!
//! # Invariants
//! - The executable mapping and the entry pointer share one owner; entries run
//!   only through the frozen `JitCtx`/`JitRet` contract.
//! - Template code allocates nothing and owns no safepoints; it exits at exact
//!   instruction boundaries, so the safepoint surface is legitimately empty.
//! - Metadata versions are validated before every entry selection, exactly as
//!   for the baseline emitter's code objects.
//!
//! # See also
//! - [`crate::baseline`] — owner of the shared entry context and epilogue
//!   status contract.

use crate::CompiledCode;
use crate::baseline::enter_compiled;
use otter_vm::native_abi::{
    BuildVersionRecord, CodeObjectMetadata, LayoutVersionRecord, VM_BUILD_VERSION,
};
use otter_vm::{JitExecOutcome, JitFunctionCode, VmRuntimeActivation};

/// Finalized template machine code for one function.
pub struct TemplateCode {
    code: CompiledCode,
    /// Frozen VM-owned metadata validated before every entry selection.
    metadata: CodeObjectMetadata,
    /// Installed code-object identity published in native frames.
    code_object_id: u64,
    /// Source function id published in this code's native frames.
    function_id: u32,
    /// Tagged register-window width published in the native frame.
    register_count: u16,
}

impl TemplateCode {
    pub(super) fn from_emission(
        code: CompiledCode,
        code_object_id: u64,
        function_id: u32,
        register_count: u16,
    ) -> Self {
        const AARCH64_TEMPLATE_ABI: u64 = 0x4136_3454_504c_0001;
        let metadata = CodeObjectMetadata {
            id: code_object_id,
            code_block_id: function_id,
            entry_offset: 0,
            code_size: code.len() as u32,
            safepoint_count: 0,
            frame_map_count: 0,
            spill_map_count: 0,
            dependency_count: 0,
            reserved: 0,
            layout: LayoutVersionRecord::CURRENT,
            build: BuildVersionRecord {
                vm_build: VM_BUILD_VERSION,
                target_abi: AARCH64_TEMPLATE_ABI,
            },
        };
        Self {
            code,
            metadata,
            code_object_id,
            function_id,
            register_count,
        }
    }

    #[cfg(test)]
    pub(super) unsafe fn entry_ptr_for_test(&self) -> *const u8 {
        // SAFETY: tests keep `self` alive for the complete native call.
        unsafe { self.code.entry_ptr() }
    }
}

impl std::fmt::Debug for TemplateCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TemplateCode")
            .field("code_len", &self.code.len())
            .finish()
    }
}

impl JitFunctionCode for TemplateCode {
    fn metadata(&self) -> CodeObjectMetadata {
        self.metadata
    }

    fn code_len(&self) -> usize {
        self.code.len()
    }

    fn run_entry(&self, activation: VmRuntimeActivation) -> JitExecOutcome {
        assert!(
            self.metadata.is_compatible_with_current_vm(),
            "incompatible native code reached entry"
        );
        // SAFETY: the mapping is live and the main entry was emitted with the
        // shared compiled-entry ABI.
        let entry = unsafe { self.code.entry_ptr() };
        // SAFETY: `entry` points into the live mapping; `activation` upholds
        // the reentry contract (valid, non-aliased for the call). Template
        // code owns no safepoints, so the frame publishes none.
        unsafe {
            enter_compiled(
                activation,
                entry,
                self.code_object_id,
                self.function_id,
                self.register_count,
                false,
            )
        }
    }
}
