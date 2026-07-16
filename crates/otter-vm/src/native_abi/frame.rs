//! Machine-visible VM thread and activation frames.
//!
//! # Contents
//! - [`VmThread`] is the only process state generated code receives.
//! - [`VmFrameHeader`] is the tier-independent frame prefix.
//! - [`NativeFrame`] is the compact activation record shared by every tier.
//!
//! # Invariants
//! - Every machine-observed field has C layout and a fixed width.
//! - `VmThread` contains addresses and stable identities, never Rust container
//!   layout. Generated code must not cast its opaque addresses to Rust types.
//! - VM and JIT are built together and consume one current layout; there is no
//!   compatibility/version protocol inside the process.
//! - Tagged values are frame-homed at safepoints; derived movable pointers are
//!   recomputed after any allocating or reentrant call.
//!
//! # See also
//! - [`super::safepoints`] for precise root maps.

use crate::Value;

/// Stable VM-thread fields visible to native code.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmThread {
    /// Address of the currently published [`NativeFrame`], or zero.
    pub current_frame: u64,
    /// Installed code generation owning `current_frame`, or zero outside JIT.
    pub current_code_object_id: u64,
    /// Opaque isolate-owned runtime context address.
    pub runtime_context: u64,
    /// Address of the active [`super::CodeRegistryView`].
    pub code_registry: u64,
    /// Address of the cooperative interrupt flag's backing byte, polled
    /// inline at every compiled back-edge.
    pub interrupt_cell: u64,
    /// Opaque isolate heap address passed to leaf runtime stubs.
    pub gc_heap: u64,
    /// Address of the back-edge fuel counter, decremented inline per
    /// back-edge; reaching zero re-enters the poll stub.
    pub backedge_fuel_cell: u64,
}

impl VmThread {
    /// Empty thread record.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            current_frame: 0,
            current_code_object_id: 0,
            runtime_context: 0,
            code_registry: 0,
            interrupt_cell: 0,
            gc_heap: 0,
            backedge_fuel_cell: 0,
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
    /// Speculative optimizing-tier frame.
    Optimizing = 2,
}

/// Bitflags attached to a native JS frame header.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeFrameFlags(u8);

impl NativeFrameFlags {
    /// Frame has precise safepoint maps for tagged machine locations.
    pub const HAS_SAFEPOINTS: u8 = 1 << 0;
    /// `activation_id` names a materialized interpreter activation. Without
    /// this bit the same word names the VM-owned frameless-call resources.
    pub const MATERIALIZED: u8 = 1 << 1;

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

/// Fixed prefix shared by interpreter, baseline, and optimizing frames.
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

/// Authoritative machine-observed synchronous activation shared by every tier.
///
/// Interpreter, baseline, and optimizer dispatch reuse the same register and
/// upvalue windows. `header.kind` changes execution mode, not activation
/// ownership. A [`crate::Frame`] exists only for interpreter execution or a
/// cold native bailout that has explicitly transferred ownership.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFrame {
    /// Common tier-independent header.
    pub header: VmFrameHeader,
    /// Base address of initialized tagged register slots.
    pub register_base: u64,
    /// Base address of a contiguous [`crate::UpvalueCell`] handle spine, or
    /// zero when `upvalue_count == 0`.
    pub upvalue_base: u64,
    /// Boxed `this` value.
    pub this_value_bits: u64,
    /// Boxed `new.target` value.
    pub new_target_bits: u64,
    /// Exact running function object used by named SELF and `arguments.callee`.
    pub self_value_bits: u64,
    /// Number of initialized handles at `upvalue_base`.
    pub upvalue_count: u32,
    /// Interpreter activation index when `MATERIALIZED` is set; otherwise the
    /// native-call owner released or materialized at the compiled-call edge.
    pub activation_id: u32,
}

impl NativeFrame {
    /// Construct the common state of a live native JS call.
    ///
    /// Upvalues and activation identity are published through the intent-level
    /// setters below before execution.
    #[must_use]
    pub const fn new(
        header: VmFrameHeader,
        register_base: u64,
        self_value: Value,
        this_value: Value,
    ) -> Self {
        Self {
            header,
            register_base,
            upvalue_base: 0,
            this_value_bits: this_value.to_abi_bits(),
            new_target_bits: Value::UNDEFINED.to_abi_bits(),
            self_value_bits: self_value.to_abi_bits(),
            upvalue_count: 0,
            activation_id: 0,
        }
    }

    /// Exact running function object.
    #[must_use]
    pub const fn self_value(&self) -> Value {
        Value::from_abi_bits(self.self_value_bits)
    }

    /// Replace the exact running function object.
    pub fn set_self_value(&mut self, value: Value) {
        self.self_value_bits = value.to_abi_bits();
    }

    /// Current `this` binding.
    #[must_use]
    pub const fn this_value(&self) -> Value {
        Value::from_abi_bits(self.this_value_bits)
    }

    /// Replace the current `this` binding.
    pub fn set_this_value(&mut self, value: Value) {
        self.this_value_bits = value.to_abi_bits();
    }

    /// Current `new.target` binding.
    #[must_use]
    pub const fn new_target(&self) -> Value {
        Value::from_abi_bits(self.new_target_bits)
    }

    /// Replace the current `new.target` binding.
    pub fn set_new_target(&mut self, value: Value) {
        self.new_target_bits = value.to_abi_bits();
    }

    /// Publish the stable handle spine consumed by native upvalue operations.
    ///
    /// The owner must keep `base..base + count * size_of::<UpvalueCell>()`
    /// initialized and alive until this native frame is no longer active.
    pub fn set_upvalue_window(&mut self, base: u64, count: u32) {
        self.upvalue_base = base;
        self.upvalue_count = count;
    }

    /// Mark this frame as the compiled view of an existing interpreter
    /// activation.
    pub fn set_materialized_activation(&mut self, activation_id: u32) {
        self.activation_id = activation_id;
        self.header.flags =
            NativeFrameFlags::from_bits(self.header.flags.bits() | NativeFrameFlags::MATERIALIZED);
    }

    /// Attach the VM owner of a frameless compiled call.
    pub fn set_native_owner(&mut self, owner_id: u32) {
        self.activation_id = owner_id;
        self.header.flags =
            NativeFrameFlags::from_bits(self.header.flags.bits() & !NativeFrameFlags::MATERIALIZED);
    }

    /// Switch this activation to interpreter dispatch without moving or
    /// copying its register/upvalue windows.
    pub fn enter_interpreter(&mut self) -> bool {
        if !self.header.flags.contains(NativeFrameFlags::MATERIALIZED) {
            return false;
        }
        self.header.kind = NativeFrameKind::Interpreter;
        true
    }

    /// Switch this activation to a compiled tier without moving or copying its
    /// register/upvalue windows.
    pub fn enter_compiled(&mut self, kind: NativeFrameKind) -> bool {
        if !matches!(
            kind,
            NativeFrameKind::Baseline | NativeFrameKind::Optimizing
        ) {
            return false;
        }
        self.header.kind = kind;
        true
    }

    /// Published interpreter activation index, when this frame entered native
    /// execution from the interpreter.
    #[must_use]
    pub fn materialized_frame_index(&self) -> Option<u32> {
        self.header
            .flags
            .contains(NativeFrameFlags::MATERIALIZED)
            .then_some(self.activation_id)
    }

    /// VM owner of this frameless native call.
    #[must_use]
    pub fn native_owner_id(&self) -> Option<u32> {
        (!self.header.flags.contains(NativeFrameFlags::MATERIALIZED)).then_some(self.activation_id)
    }
}

const _: [(); 56] = [(); std::mem::size_of::<VmThread>()];
const _: [(); 8] = [(); std::mem::align_of::<VmThread>()];
const _: [(); 16] = [(); std::mem::size_of::<VmFrameHeader>()];
const _: [(); 64] = [(); std::mem::size_of::<NativeFrame>()];
const _: [(); 8] = [(); std::mem::align_of::<NativeFrame>()];
const _: [(); 0] = [(); std::mem::offset_of!(VmThread, current_frame)];
const _: [(); 8] = [(); std::mem::offset_of!(VmThread, current_code_object_id)];
const _: [(); 40] = [(); std::mem::offset_of!(VmThread, gc_heap)];
const _: [(); 48] = [(); std::mem::offset_of!(VmThread, backedge_fuel_cell)];
const _: [(); 8] = [(); std::mem::offset_of!(VmFrameHeader, pc)];
const _: [(); 12] = [(); std::mem::offset_of!(VmFrameHeader, register_count)];
const _: [(); 15] = [(); std::mem::offset_of!(VmFrameHeader, flags)];
const _: [(); 16] = [(); std::mem::offset_of!(NativeFrame, register_base)];
const _: [(); 24] = [(); std::mem::offset_of!(NativeFrame, upvalue_base)];
const _: [(); 32] = [(); std::mem::offset_of!(NativeFrame, this_value_bits)];
const _: [(); 40] = [(); std::mem::offset_of!(NativeFrame, new_target_bits)];
const _: [(); 48] = [(); std::mem::offset_of!(NativeFrame, self_value_bits)];
const _: [(); 56] = [(); std::mem::offset_of!(NativeFrame, upvalue_count)];
const _: [(); 60] = [(); std::mem::offset_of!(NativeFrame, activation_id)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_thread_has_no_published_activation() {
        let thread = VmThread::empty();
        assert_eq!(thread.current_frame, 0);
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

    #[test]
    fn native_frame_identity_distinguishes_native_owner_from_activation() {
        let mut frame = NativeFrame::new(
            VmFrameHeader::interpreter(7, 3),
            0x1000,
            Value::function(7),
            Value::number_i32(4),
        );
        assert_eq!(frame.self_value(), Value::function(7));
        assert_eq!(frame.this_value(), Value::number_i32(4));
        assert_eq!(frame.new_target(), Value::undefined());
        frame.set_native_owner(9);
        assert_eq!(frame.native_owner_id(), Some(9));
        assert_eq!(frame.materialized_frame_index(), None);
        frame.set_materialized_activation(4);
        assert_eq!(frame.materialized_frame_index(), Some(4));
        assert_eq!(frame.native_owner_id(), None);
    }

    #[test]
    fn tier_switches_keep_one_native_activation_and_window() {
        let mut slots = [Value::number_i32(1), Value::number_i32(2)];
        let base = slots.as_mut_ptr() as u64;
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 7,
                code_block_id: 7,
                pc: 11,
                register_count: slots.len() as u16,
                kind: NativeFrameKind::Baseline,
                flags: NativeFrameFlags::empty(),
            },
            base,
            Value::function(7),
            Value::undefined(),
        );
        frame.set_materialized_activation(3);
        assert!(frame.enter_interpreter());
        assert_eq!(frame.header.kind, NativeFrameKind::Interpreter);
        assert_eq!(frame.register_base, base);
        assert_eq!(slots, [Value::number_i32(1), Value::number_i32(2)]);

        assert!(frame.enter_compiled(NativeFrameKind::Optimizing));
        assert_eq!(frame.header.kind, NativeFrameKind::Optimizing);
        assert_eq!(frame.register_base, base);
        assert_eq!(slots, [Value::number_i32(1), Value::number_i32(2)]);
    }

    #[test]
    fn native_frame_is_one_cache_line() {
        assert_eq!(std::mem::size_of::<NativeFrame>(), 64);
        assert_eq!(std::mem::offset_of!(NativeFrame, register_base), 16);
        assert_eq!(std::mem::offset_of!(NativeFrame, upvalue_base), 24);
        assert_eq!(std::mem::offset_of!(NativeFrame, self_value_bits), 48);
        assert_eq!(std::mem::offset_of!(NativeFrame, activation_id), 60);
    }
}
