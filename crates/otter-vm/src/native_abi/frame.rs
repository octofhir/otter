//! Stable machine-visible VM thread and activation frames.
//!
//! # Contents
//! - [`VmThread`] is the only process state generated code receives.
//! - [`VmFrameHeader`] is the tier-independent frame prefix.
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
    /// Address of the cooperative interrupt flag's backing byte, polled
    /// inline at every compiled back-edge.
    pub interrupt_cell: u64,
    /// Rooted pending exception as boxed `Value` bits, or zero when absent.
    pub pending_exception_bits: u64,
    /// Current code invalidation epoch.
    pub code_epoch: u64,
    /// Opaque isolate heap address passed to leaf runtime stubs.
    pub gc_heap: u64,
    /// Address of the back-edge fuel counter, decremented inline per
    /// back-edge; reaching zero re-enters the poll stub.
    pub backedge_fuel_cell: u64,
    /// Address of the shared synchronous native-reentry depth counter.
    pub sync_reentry_depth_cell: u64,
    /// VM-native layout version observed by entered code.
    pub layout_version: u32,
    /// Runtime-stub inventory version observed by entered code.
    pub stub_table_version: u32,
    /// Effective synchronous-reentry limit checked before a frameless native
    /// call mutates state.
    pub sync_reentry_limit: u32,
    /// Reserved; zero in version 1.
    pub reserved0: u32,
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
            gc_heap: 0,
            backedge_fuel_cell: 0,
            sync_reentry_depth_cell: 0,
            layout_version: VM_LAYOUT_VERSION,
            stub_table_version: RUNTIME_STUB_TABLE_VERSION,
            sync_reentry_limit: 0,
            reserved0: 0,
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
pub struct NativeFrameFlags(u8);

impl NativeFrameFlags {
    /// Frame has precise safepoint maps for tagged machine locations.
    pub const HAS_SAFEPOINTS: u8 = 1 << 0;
    /// Frame may call back into JS from a runtime stub.
    pub const MAY_REENTER_JS: u8 = 1 << 1;

    /// Empty flag set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Build from raw bits.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// Raw flag bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether all `mask` bits are present.
    #[must_use]
    pub const fn contains(self, mask: u8) -> bool {
        self.0 & mask == mask
    }
}

/// Fixed prefix shared by interpreter, baseline, and runtime-stub frames.
#[repr(C, align(4))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmFrameHeader {
    /// Global VM function id.
    pub function_id: u32,
    /// Immutable code-block id.
    pub code_block_id: u32,
    /// Canonical instruction-index resume PC.
    pub pc: u32,
    /// Number of initialized tagged register slots.
    pub register_count: u16,
    /// Execution tier owning the frame.
    pub kind: NativeFrameKind,
    /// Frame flags.
    pub flags: NativeFrameFlags,
}

impl VmFrameHeader {
    /// Interpreter-owned frame header at function entry.
    #[must_use]
    pub const fn interpreter(function_id: u32, register_count: u16) -> Self {
        Self {
            function_id,
            code_block_id: function_id,
            pc: 0,
            register_count,
            kind: NativeFrameKind::Interpreter,
            flags: NativeFrameFlags::empty(),
        }
    }
}

/// Authoritative machine-observed synchronous activation.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFrame {
    /// Common tier-independent header.
    pub header: VmFrameHeader,
    /// Address of the previous frame, or zero at the stack root.
    pub previous_frame: u64,
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
    /// Number of argument slots.
    pub argument_count: u16,
    /// Reserved; zero in version 1.
    pub reserved0: u16,
    /// Dense feedback-vector identity.
    pub feedback_id: u32,
}

const _: [(); 96] = [(); std::mem::size_of::<VmThread>()];
const _: [(); 8] = [(); std::mem::align_of::<VmThread>()];
const _: [(); 16] = [(); std::mem::size_of::<VmFrameHeader>()];
const _: [(); 88] = [(); std::mem::size_of::<NativeFrame>()];
const _: [(); 8] = [(); std::mem::align_of::<NativeFrame>()];
const _: [(); 0] = [(); std::mem::offset_of!(VmThread, current_frame)];
const _: [(); 40] = [(); std::mem::offset_of!(VmThread, pending_exception_bits)];
const _: [(); 56] = [(); std::mem::offset_of!(VmThread, gc_heap)];
const _: [(); 64] = [(); std::mem::offset_of!(VmThread, backedge_fuel_cell)];
const _: [(); 72] = [(); std::mem::offset_of!(VmThread, sync_reentry_depth_cell)];
const _: [(); 80] = [(); std::mem::offset_of!(VmThread, layout_version)];
const _: [(); 88] = [(); std::mem::offset_of!(VmThread, sync_reentry_limit)];
const _: [(); 8] = [(); std::mem::offset_of!(VmFrameHeader, pc)];
const _: [(); 12] = [(); std::mem::offset_of!(VmFrameHeader, register_count)];
const _: [(); 15] = [(); std::mem::offset_of!(VmFrameHeader, flags)];
const _: [(); 24] = [(); std::mem::offset_of!(NativeFrame, register_base)];
const _: [(); 48] = [(); std::mem::offset_of!(NativeFrame, code_object_id)];
const _: [(); 72] = [(); std::mem::offset_of!(NativeFrame, return_register)];
const _: [(); 84] = [(); std::mem::offset_of!(NativeFrame, feedback_id)];

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

    #[test]
    fn interpreter_header_uses_common_layout() {
        let header = VmFrameHeader::interpreter(7, 23);
        assert_eq!(header.function_id, 7);
        assert_eq!(header.code_block_id, 7);
        assert_eq!(header.pc, 0);
        assert_eq!(header.register_count, 23);
        assert_eq!(header.kind, NativeFrameKind::Interpreter);
        assert_eq!(header.flags, NativeFrameFlags::empty());
    }
}
