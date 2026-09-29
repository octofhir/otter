//! Machine-visible VM thread and activation frames.
//!
//! # Contents
//! - [`VmThread`] is the only process state generated code receives.
//! - [`VmFrameHeader`] is the tier-independent frame prefix.
//! - [`NativeFrame`] is the compact activation record shared by every tier;
//!   records link to their callers into one frame chain.
//! - `NATIVE_FRAME_*_OFFSET` constants give generated code the byte offset of
//!   every field it reads or writes.
//! - [`NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET`] locates the lazy arguments root.
//!
//! # Invariants
//! - Every machine-observed field has C layout and a fixed width.
//! - `VmThread` contains addresses and stable identities, never Rust container
//!   layout. Generated code must not cast its opaque addresses to Rust types.
//! - VM and JIT are built together and consume one current layout; there is no
//!   compatibility/version protocol inside the process.
//! - A frame carries no binding storage. Captured bindings live in contexts
//!   reached through registers; the incoming context is `SELF`'s closure
//!   context, so SELF is always the exact closure being executed.
//! - The nullable arguments cache is initialized before publication and traced
//!   in place; exact deopt preserves its identity in the interpreter frame.
//! - Published frames form one chain from the innermost frame through
//!   [`NativeFrame::caller`] across every nested compiled entry. Nothing else
//!   records the live frame set: collectors, stack snapshots and deopt walk it.
//!   The innermost frame of the live compiled entry is the entry's
//!   `JitCtx::native_frame`; [`VmThread::frame_cell`] names that cell.
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
    /// Address of the cell holding the innermost published [`NativeFrame`]
    /// of this compiled entry (the entry's `JitCtx::native_frame`), or zero.
    /// Generated linkage updates the cell; runtime stubs read the current
    /// frame and its generation through it.
    pub frame_cell: u64,
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
    /// Address of the isolate's global-declarative-record epoch. Generated
    /// global-object reads compare the live epoch with their compile snapshot
    /// before trusting a shape-matched own-data slot.
    pub global_lexical_epoch_cell: u64,
    /// Address of the collector's incremental-marking flag byte. Generated
    /// code runs the generational write barrier inline and reaches the runtime
    /// only when this byte says a marking cycle is in progress.
    pub marking_flag_cell: u64,
    /// Address of the isolate's array-index accessor protector byte (non-zero
    /// once any indexed accessor exists anywhere). Generated code tests it
    /// before a leaf entry that creates an array index, such as `push`.
    pub array_index_protector_cell: u64,
    /// Address of the isolate's active realm id (`u32`). Generated proofs
    /// that resolve a prototype in the active realm compare it first.
    pub active_realm_cell: u64,
    /// Address of the isolate's ArrayBuffer detach protector byte (non-zero
    /// once any buffer was detached). Generated typed-array access uses a
    /// view's cached element base only while it reads zero.
    pub array_buffer_detach_protector_cell: u64,
}

impl VmThread {
    /// Empty thread record.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            frame_cell: 0,
            runtime_context: 0,
            code_registry: 0,
            interrupt_cell: 0,
            gc_heap: 0,
            backedge_fuel_cell: 0,
            global_lexical_epoch_cell: 0,
            marking_flag_cell: 0,
            array_index_protector_cell: 0,
            active_realm_cell: 0,
            array_buffer_detach_protector_cell: 0,
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
    /// `register_base` points into generated code's native stack instead of
    /// the VM register arena. The published native activation therefore owns
    /// precise tracing and in-place rewriting of this register window.
    ///
    /// Their generated call sequence owns synchronous-depth and
    /// native-stack-byte accounting until return, throw, or cold
    /// deoptimization completes. Frames without this bit use the materialized
    /// activation named by [`crate::jit::VmRuntimeActivation`].
    pub const STACK_REGISTERS: u8 = 1 << 1;
    /// Activation uses derived-constructor return and `this` binding
    /// semantics. The bit is representation-neutral: materialized entries and
    /// generated stack calls publish the same fact.
    pub const DERIVED_CONSTRUCTOR: u8 = 1 << 2;
    /// The generated caller published every actual argument of this call in
    /// a tagged window that starts right after the register window and holds
    /// [`NativeFrame::argument_count`] values. Only stack-owned frames carry
    /// the bit: a materialized activation keeps the same list in its cold
    /// record instead.
    pub const INCOMING_ARGUMENTS: u8 = 1 << 3;
    /// The optimizing tier is entered at a loop header rather than at the
    /// function's start: the header's canonical PC is the frame's `pc`, and
    /// every live register holds the interpreter's current value. The
    /// generated entry dispatch reads the bit once to select the header's OSR
    /// block; nothing else consults it.
    pub const OSR_ENTRY: u8 = 1 << 4;

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
            pc: 0,
            register_count,
            kind: NativeFrameKind::Interpreter,
            flags: NativeFrameFlags::empty(),
        }
    }
}

/// Authoritative machine-observed synchronous activation shared by every tier.
///
/// Interpreter, baseline, and optimizer dispatch reuse the same register
/// window. `header.kind` changes execution mode, not activation
/// ownership. A [`crate::Frame`] exists only for interpreter execution or a
/// cold native bailout that has explicitly transferred ownership.
#[repr(C, align(8))]
#[derive(Debug, PartialEq, Eq)]
pub struct NativeFrame {
    /// Common tier-independent header.
    pub header: VmFrameHeader,
    /// Installed code generation executing this frame, or zero for an
    /// interpreter-owned record. Stubs resolve the frame's safepoints and
    /// inline recipes through it.
    pub code_object_id: u32,
    /// Base address of initialized tagged register slots.
    pub register_base: u64,
    /// Boxed `this` value.
    pub this_value_bits: u64,
    /// Boxed `new.target` value.
    pub new_target_bits: u64,
    /// Exact running function object (SELF): the closure whose context
    /// `LoadClosureContext` reads, also `LoadSelf` and `arguments.callee`.
    pub self_value_bits: u64,
    /// Number of actual arguments published after the register window when
    /// [`NativeFrameFlags::INCOMING_ARGUMENTS`] is set; otherwise unused.
    pub argument_count: u32,
    /// Lazily materialized arguments identity. Null means the actual window
    /// remains authoritative.
    pub(crate) arguments_object: crate::object::JsObject,
    /// Calling frame's record, or null for the outermost published frame.
    /// Linkage writes it before the frame becomes the innermost one.
    pub caller: u64,
    /// Stack-register frames in the chain from this record outward, this one
    /// included. The logical JavaScript depth of generated frames; linkage
    /// computes it from the caller's value.
    pub depth: u32,
}

impl NativeFrame {
    /// Construct the common state of a live native JS call.
    ///
    /// Register-window ownership is published through the intent-level
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
            code_object_id: 0,
            register_base,
            this_value_bits: this_value.to_abi_bits(),
            new_target_bits: Value::UNDEFINED.to_abi_bits(),
            self_value_bits: self_value.to_abi_bits(),
            argument_count: 0,
            arguments_object: crate::object::JsObject::null(),
            caller: 0,
            depth: 0,
        }
    }

    /// The activation's arguments identity after observable materialization.
    pub(crate) fn arguments_object(&self) -> Option<crate::object::JsObject> {
        (!self.arguments_object.is_null()).then_some(self.arguments_object)
    }

    /// Publish the identity in the collector-traced native frame slot.
    pub(crate) fn set_arguments_object(&mut self, object: Option<crate::object::JsObject>) {
        self.arguments_object = object.unwrap_or_else(crate::object::JsObject::null);
    }

    /// Calling frame's record, or null for the outermost published frame.
    #[must_use]
    pub fn caller_frame(&self) -> *mut NativeFrame {
        self.caller as *mut NativeFrame
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

    /// Mark the register window as generated-code stack storage.
    ///
    /// The frame must remain published while generated code or a cold
    /// interpreter continuation can allocate. Publication makes every tagged
    /// slot in the window a precise, rewriteable collector root.
    pub fn set_stack_registers(&mut self) {
        self.header.flags = NativeFrameFlags::from_bits(
            self.header.flags.bits() | NativeFrameFlags::STACK_REGISTERS,
        );
    }

    /// Publish the actual-argument window that follows the register window.
    ///
    /// The owner must keep `register_base + register_count * 8` onward
    /// initialized with `count` tagged values while this frame is active. The
    /// register window itself must be generated-code stack storage.
    pub fn set_incoming_arguments(&mut self, count: u32) {
        debug_assert!(
            self.header
                .flags
                .contains(NativeFrameFlags::STACK_REGISTERS),
            "incoming arguments are published only by stack-owned frames"
        );
        self.argument_count = count;
        self.header.flags = NativeFrameFlags::from_bits(
            self.header.flags.bits() | NativeFrameFlags::INCOMING_ARGUMENTS,
        );
    }

    /// Number of published actual arguments, or `None` when the caller kept
    /// them elsewhere.
    #[must_use]
    pub const fn incoming_argument_count(&self) -> Option<u32> {
        if self
            .header
            .flags
            .contains(NativeFrameFlags::INCOMING_ARGUMENTS)
        {
            Some(self.argument_count)
        } else {
            None
        }
    }

    /// Mark this activation as a derived constructor.
    pub fn set_derived_constructor(&mut self) {
        self.header.flags = NativeFrameFlags::from_bits(
            self.header.flags.bits() | NativeFrameFlags::DERIVED_CONSTRUCTOR,
        );
    }

    /// Whether this activation owns derived-constructor semantics.
    #[must_use]
    pub const fn is_derived_constructor(&self) -> bool {
        self.header
            .flags
            .contains(NativeFrameFlags::DERIVED_CONSTRUCTOR)
    }

    /// Switch this activation to interpreter dispatch without moving or
    /// copying its register window.
    pub fn enter_interpreter(&mut self) -> bool {
        if self
            .header
            .flags
            .contains(NativeFrameFlags::STACK_REGISTERS)
        {
            return false;
        }
        self.header.kind = NativeFrameKind::Interpreter;
        true
    }

    /// Switch this activation to a compiled tier without moving or copying its
    /// register window.
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
}

const _: [(); 88] = [(); std::mem::size_of::<VmThread>()];
const _: [(); 8] = [(); std::mem::align_of::<VmThread>()];
const _: [(); 12] = [(); std::mem::size_of::<VmFrameHeader>()];
const _: [(); 72] = [(); std::mem::size_of::<NativeFrame>()];
const _: [(); 8] = [(); std::mem::align_of::<NativeFrame>()];
const _: [(); 0] = [(); std::mem::offset_of!(VmThread, frame_cell)];
const _: [(); 32] = [(); std::mem::offset_of!(VmThread, gc_heap)];
const _: [(); 40] = [(); std::mem::offset_of!(VmThread, backedge_fuel_cell)];
const _: [(); 48] = [(); std::mem::offset_of!(VmThread, global_lexical_epoch_cell)];
const _: [(); 56] = [(); std::mem::offset_of!(VmThread, marking_flag_cell)];
const _: [(); 64] = [(); std::mem::offset_of!(VmThread, array_index_protector_cell)];
const _: [(); 72] = [(); std::mem::offset_of!(VmThread, active_realm_cell)];
const _: [(); 80] = [(); std::mem::offset_of!(VmThread, array_buffer_detach_protector_cell)];
const _: [(); 4] = [(); std::mem::offset_of!(VmFrameHeader, pc)];
const _: [(); 8] = [(); std::mem::offset_of!(VmFrameHeader, register_count)];
const _: [(); 11] = [(); std::mem::offset_of!(VmFrameHeader, flags)];
const _: [(); 16] = [(); std::mem::offset_of!(NativeFrame, register_base)];
const _: [(); 24] = [(); std::mem::offset_of!(NativeFrame, this_value_bits)];
const _: [(); 32] = [(); std::mem::offset_of!(NativeFrame, new_target_bits)];
const _: [(); 40] = [(); std::mem::offset_of!(NativeFrame, self_value_bits)];
const _: [(); 48] = [(); std::mem::offset_of!(NativeFrame, argument_count)];
const _: [(); 12] = [(); std::mem::offset_of!(NativeFrame, code_object_id)];
const _: [(); 56] = [(); std::mem::offset_of!(NativeFrame, caller)];
const _: [(); 64] = [(); std::mem::offset_of!(NativeFrame, depth)];

/// Byte offset of [`NativeFrame::code_object_id`]. It shares the eight-byte
/// word that starts at the register count, so linkage writes the register
/// shape, tier, flags and generation with one store.
pub const NATIVE_FRAME_CODE_OBJECT_ID_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, code_object_id) as u32;
/// Byte offset of [`NativeFrame::caller`].
pub const NATIVE_FRAME_CALLER_OFFSET: u32 = std::mem::offset_of!(NativeFrame, caller) as u32;
/// Byte offset of [`NativeFrame::depth`].
pub const NATIVE_FRAME_DEPTH_OFFSET: u32 = std::mem::offset_of!(NativeFrame, depth) as u32;

/// Byte offset of [`NativeFrame::register_base`].
pub const NATIVE_FRAME_REGISTER_BASE_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, register_base) as u32;
/// Byte offset of [`NativeFrame::this_value_bits`].
pub const NATIVE_FRAME_THIS_OFFSET: u32 = std::mem::offset_of!(NativeFrame, this_value_bits) as u32;
/// Byte offset of [`NativeFrame::new_target_bits`].
pub const NATIVE_FRAME_NEW_TARGET_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, new_target_bits) as u32;
/// Byte offset of [`NativeFrame::self_value_bits`] (SELF). Generated
/// `LoadClosureContext` loads the closure here, then its context word.
pub const NATIVE_FRAME_SELF_OFFSET: u32 = std::mem::offset_of!(NativeFrame, self_value_bits) as u32;

/// Byte offset of [`NativeFrame::argument_count`], written by generated
/// callers that publish the actual-argument window.
pub const NATIVE_FRAME_ARGUMENT_COUNT_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, argument_count) as u32;

/// Nullable arguments object root; zero admits direct actual-window reads.
pub const NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET: u32 =
    std::mem::offset_of!(NativeFrame, arguments_object) as u32;
const _: [(); 52] = [(); std::mem::offset_of!(NativeFrame, arguments_object)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_thread_has_no_published_activation() {
        let thread = VmThread::empty();
        assert_eq!(thread.frame_cell, 0);
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
        assert_eq!(header.pc, 0);
        assert_eq!(header.register_count, 23);
        assert_eq!(header.kind, NativeFrameKind::Interpreter);
        assert_eq!(header.flags, NativeFrameFlags::empty());
    }

    #[test]
    fn native_frame_identity_marks_stack_storage() {
        let mut frame = NativeFrame::new(
            VmFrameHeader::interpreter(7, 3),
            0x1000,
            Value::function(7),
            Value::number_i32(4),
        );
        assert_eq!(frame.self_value(), Value::function(7));
        assert_eq!(frame.this_value(), Value::number_i32(4));
        assert_eq!(frame.new_target(), Value::undefined());
        frame.set_stack_registers();
        assert!(
            frame
                .header
                .flags
                .contains(NativeFrameFlags::STACK_REGISTERS)
        );
        assert_eq!(frame.incoming_argument_count(), None);
        frame.set_incoming_arguments(5);
        assert_eq!(frame.incoming_argument_count(), Some(5));
    }

    #[test]
    fn tier_switches_keep_one_native_activation_and_window() {
        let mut slots = [Value::number_i32(1), Value::number_i32(2)];
        let base = slots.as_mut_ptr() as u64;
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 7,
                pc: 11,
                register_count: slots.len() as u16,
                kind: NativeFrameKind::Baseline,
                flags: NativeFrameFlags::empty(),
            },
            base,
            Value::function(7),
            Value::undefined(),
        );
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
    fn native_frame_layout_holds_no_binding_storage() {
        assert_eq!(std::mem::size_of::<VmFrameHeader>(), 12);
        assert_eq!(std::mem::size_of::<NativeFrame>(), 72);
        assert_eq!(NATIVE_FRAME_REGISTER_BASE_OFFSET, 16);
        assert_eq!(NATIVE_FRAME_THIS_OFFSET, 24);
        assert_eq!(NATIVE_FRAME_NEW_TARGET_OFFSET, 32);
        assert_eq!(NATIVE_FRAME_SELF_OFFSET, 40);
        assert_eq!(NATIVE_FRAME_ARGUMENT_COUNT_OFFSET, 48);
        assert_eq!(NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET, 52);
        assert_eq!(NATIVE_FRAME_CALLER_OFFSET, 56);
        assert_eq!(NATIVE_FRAME_DEPTH_OFFSET, 64);
    }
}
