//! Stable machine-visible VM thread and activation frames.
//!
//! # Contents
//! - [`VmThread`] is the only process state generated code receives.
//! - [`NativeFrameHeader`] is the tier-independent frame prefix.
//! - [`NativeFrame`] publishes tagged slots and code identity.
//!
//! # Invariants
//! - Every machine-observed field has C layout and a fixed width.
//! - `VmThread` contains addresses and stable identities, never Rust container
//!   layout. Generated code must not cast its opaque addresses to Rust types.
//! - Tagged values are frame-homed at safepoints; derived movable pointers are
//!   recomputed after any allocating or reentrant call.
//!
//! # See also
//! - [`super::safepoints`] for precise root maps.
//! - [`super::metadata`] for layout and code-object versions.

use super::metadata::{RUNTIME_STUB_TABLE_VERSION, VM_LAYOUT_VERSION};

/// Stable VM-thread fields visible to native code.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmThread {
    /// Address of the currently published [`NativeFrame`], or zero.
    pub current_frame: u64,
    /// Opaque isolate-owned runtime context address.
    pub runtime_context: u64,
    /// Address of the authoritative [`super::RuntimeStubTable`].
    pub runtime_stub_table: u64,
    /// Address of the active [`super::CodeRegistryView`].
    pub code_registry: u64,
    /// Address of the interrupt/budget cell.
    pub interrupt_cell: u64,
    /// Rooted pending exception as boxed `Value` bits, or zero when absent.
    pub pending_exception_bits: u64,
    /// Current code invalidation epoch.
    pub code_epoch: u64,
    /// VM-native layout version observed by entered code.
    pub layout_version: u32,
    /// Runtime-stub inventory version observed by entered code.
    pub stub_table_version: u32,
}

impl VmThread {
    /// Empty thread record carrying the current ABI versions.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            current_frame: 0,
            runtime_context: 0,
            runtime_stub_table: 0,
            code_registry: 0,
            interrupt_cell: 0,
            pending_exception_bits: 0,
            code_epoch: 0,
            layout_version: VM_LAYOUT_VERSION,
            stub_table_version: RUNTIME_STUB_TABLE_VERSION,
        }
    }
}

/// Execution tier that owns a frame.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFrameKind {
    /// Bytecode interpreter frame.
    Interpreter = 0,
    /// Low-latency baseline compiled frame.
    Baseline = 1,
    /// Runtime stub frame visible to GC and diagnostics.
    RuntimeStub = 2,
}

/// Bitflags attached to a native JS frame header.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeFrameFlags(u32);

impl NativeFrameFlags {
    /// Frame has precise safepoint maps for tagged machine locations.
    pub const HAS_SAFEPOINTS: u32 = 1 << 0;
    /// Frame may call back into JS from a runtime stub.
    pub const MAY_REENTER_JS: u32 = 1 << 1;

    /// Empty flag set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Build from raw bits.
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Raw flag bits.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether all `mask` bits are present.
    #[must_use]
    pub const fn contains(self, mask: u32) -> bool {
        self.0 & mask == mask
    }
}

/// Fixed prefix shared by interpreter, baseline, and runtime-stub frames.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFrameHeader {
    /// Address of the previous frame, or zero at the stack root.
    pub previous_frame: u64,
    /// Global VM function id.
    pub function_id: u32,
    /// Immutable code-block id.
    pub code_block_id: u32,
    /// Canonical instruction-index resume PC.
    pub resume_pc: u32,
    /// Execution tier owning the frame.
    pub kind: NativeFrameKind,
    /// Reserved; zero in layout version 2.
    pub reserved0: [u8; 3],
    /// Frame flags.
    pub flags: NativeFrameFlags,
    /// Number of initialized tagged register slots.
    pub register_count: u16,
    /// Number of argument slots.
    pub argument_count: u16,
    /// Dense feedback-vector identity.
    pub feedback_id: u32,
}

/// Authoritative machine-observed synchronous activation.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFrame {
    /// Common tier-independent header.
    pub header: NativeFrameHeader,
    /// Base address of initialized tagged register slots.
    pub register_base: u64,
    /// Base address of overflow arguments, or zero.
    pub argument_base: u64,
    /// Isolate-local feedback-vector base, or zero.
    pub feedback_base: u64,
    /// Installed code-object identity, or zero for interpreter frames.
    pub code_object_id: u64,
    /// Boxed `this` value.
    pub this_value_bits: u64,
    /// Boxed `new.target` value.
    pub new_target_bits: u64,
    /// Caller destination register; `u32::MAX` at the stack root.
    pub return_register: u32,
    /// Index into cold async/generator/protocol state, or `u32::MAX`.
    pub cold_state_index: u32,
}

const _: [(); 64] = [(); std::mem::size_of::<VmThread>()];
const _: [(); 8] = [(); std::mem::align_of::<VmThread>()];
const _: [(); 40] = [(); std::mem::size_of::<NativeFrameHeader>()];
const _: [(); 96] = [(); std::mem::size_of::<NativeFrame>()];
const _: [(); 8] = [(); std::mem::align_of::<NativeFrame>()];
const _: [(); 0] = [(); std::mem::offset_of!(VmThread, current_frame)];
const _: [(); 40] = [(); std::mem::offset_of!(VmThread, pending_exception_bits)];
const _: [(); 56] = [(); std::mem::offset_of!(VmThread, layout_version)];
const _: [(); 16] = [(); std::mem::offset_of!(NativeFrameHeader, resume_pc)];
const _: [(); 24] = [(); std::mem::offset_of!(NativeFrameHeader, flags)];
const _: [(); 32] = [(); std::mem::offset_of!(NativeFrameHeader, feedback_id)];
const _: [(); 40] = [(); std::mem::offset_of!(NativeFrame, register_base)];
const _: [(); 64] = [(); std::mem::offset_of!(NativeFrame, code_object_id)];
const _: [(); 88] = [(); std::mem::offset_of!(NativeFrame, return_register)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_thread_uses_current_versions() {
        let thread = VmThread::empty();
        assert_eq!(thread.layout_version, VM_LAYOUT_VERSION);
        assert_eq!(thread.stub_table_version, RUNTIME_STUB_TABLE_VERSION);
    }

    #[test]
    fn frame_flags_round_trip() {
        let flags = NativeFrameFlags::from_bits(NativeFrameFlags::HAS_SAFEPOINTS);
        assert!(flags.contains(NativeFrameFlags::HAS_SAFEPOINTS));
        assert_eq!(flags.bits(), NativeFrameFlags::HAS_SAFEPOINTS);
    }
}
