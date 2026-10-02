//! Machine-visible VM thread and activation frames.
//!
//! # Contents
//! - [`VmThread`] is the only process state generated code receives.
//! - [`VmFrameHeader`] is the tier-independent frame prefix.
//! - [`Frame`] is the compact activation record shared by every tier;
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
//!   [`Frame::caller`] across every nested compiled entry. Nothing else
//!   records the live frame set: collectors, stack snapshots and deopt walk it.
//!   The innermost frame of the live compiled entry is the entry's
//!   `JitCtx::native_frame`; [`VmThread::frame_cell`] names that cell.
//! - Tagged values are frame-homed at safepoints; derived movable pointers are
//!   recomputed after any allocating or reentrant call. An optimized frame
//!   names its in-progress call's safepoint in its own record; nothing else
//!   publishes its roots.
//!
//! # See also
//! - [`super::safepoints`] for precise root maps.

use crate::{RegisterWindow, Value, cold_frame::ColdFrameIdx};
use otter_gc::raw::{RawGc, SlotVisitor};

/// Stable VM-thread fields visible to native code.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmThread {
    /// Address of the cell holding the innermost published [`Frame`]
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
    /// 4 GiB-aligned base of the GC cage. Compressed handles decompress as
    /// `cage_base | offset`.
    pub cage_base: u64,
}

impl VmThread {
    /// Thread record with no published services; only the process cage base
    /// is filled in.
    #[must_use]
    pub fn empty() -> Self {
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
            cage_base: otter_gc::cage_base() as u64,
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
    /// Host-call frame published by the call trampoline for a callee that is
    /// not a bytecode function. `header.function_id` holds its
    /// [`HostCallKind`] and `header.pc` its resumption state; its register
    /// prefix stages values for a child call and its actual window owns the
    /// arguments.
    Host = 3,
}

/// Callee family the call trampoline selected for a [`NativeFrameKind::Host`]
/// frame. The host entry dispatches on this code only.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCallKind {
    /// A native function body.
    Native = 0,
    /// A proxy exotic object.
    Proxy = 1,
    /// Any other value: an object with an internal native `[[Call]]`, or a
    /// value that is not callable.
    Other = 2,
    /// A class constructor reached through `[[Call]]`.
    ClassCall = 3,
    /// A bytecode function without `[[Construct]]` reached through `new`.
    NotConstructor = 4,
}

impl HostCallKind {
    /// Decode the trampoline-written code.
    #[must_use]
    pub const fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Native,
            1 => Self::Proxy,
            2 => Self::Other,
            3 => Self::ClassCall,
            4 => Self::NotConstructor,
            _ => return None,
        })
    }
}

/// Register slots of every host frame. A host body stages a child call's
/// callee, receiver and up to two actuals here, where they are traced.
pub const HOST_FRAME_REGISTER_COUNT: u16 = 6;

/// Bitflags attached to a native JS frame header.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeFrameFlags(u8);

impl NativeFrameFlags {
    /// Frame has precise safepoint maps for tagged machine locations.
    pub const HAS_SAFEPOINTS: u8 = 1 << 0;
    /// Logical execution has returned; assembly still owns this physical extent.
    pub const COMPLETED: u8 = 1 << 1;
    /// Request a physical tail replacement at the trampoline boundary.
    pub const TAIL_CALL: u8 = 1 << 6;
    /// Transfer the current activation to a selected tier entry.
    /// This bit belongs only to a call request, never a published frame.
    pub const TIER_ENTRY: u8 = 1 << 7;
    /// Activation uses derived-constructor return and `this` binding
    /// semantics across every execution tier.
    pub const DERIVED_CONSTRUCTOR: u8 = 1 << 2;
    /// An interpreter activation waiting on a call it staged stands at the
    /// staging instruction; a normal completion of the call moves it past
    /// that instruction. Without the bit the instruction runs again (a
    /// protocol ladder resuming its parked state).
    pub const ADVANCE_ON_RESUME: u8 = 1 << 3;
    /// The optimizing tier is entered at a loop header rather than at the
    /// function's start: the header's canonical PC is the frame's `pc`, and
    /// every live register holds the interpreter's current value. The
    /// generated entry dispatch reads the bit once to select the header's OSR
    /// block; nothing else consults it.
    pub const OSR_ENTRY: u8 = 1 << 4;
    /// This activation was entered through `[[Construct]]`; a primitive return
    /// yields its receiver after derived-constructor checks.
    pub const CONSTRUCT: u8 = 1 << 5;

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

    /// This set with `mask` added.
    #[must_use]
    pub const fn with(self, mask: u8) -> Self {
        Self(self.0 | mask)
    }

    /// This set with `mask` removed.
    #[must_use]
    pub const fn without(self, mask: u8) -> Self {
        Self(self.0 & !mask)
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
/// ownership. Runtime operations read and mutate this record directly.
#[repr(C, align(8))]
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    /// Common tier-independent header.
    pub header: VmFrameHeader,
    /// Installed code generation executing this frame, or zero for an
    /// interpreter-owned record. Stubs resolve the frame's safepoints and
    /// inline recipes through it.
    pub code_object_id: u32,
    /// Interpreter register window: `header.register_count` initialized
    /// tagged slots, or empty for an optimizing frame whose values live in
    /// its safepoint root homes.
    pub registers: RegisterWindow,
    /// Boxed `this` value.
    pub this_value: Value,
    /// Boxed `new.target` value.
    pub new_target_value: Value,
    /// Exact running function object (SELF): the closure whose context
    /// `LoadClosureContext` reads, also `LoadSelf` and `arguments.callee`.
    pub self_value: Value,
    /// Number of initialized actual arguments at [`Self::actuals`].
    pub argument_count: u32,
    /// Lazily materialized arguments identity. Null means the actual window
    /// remains authoritative.
    pub(crate) arguments_object: crate::object::JsObject,
    /// First of `argument_count` initialized actual arguments. A generated
    /// caller's outgoing span, read in place; the trampoline's copy for
    /// interpreter and host frames. The window owner keeps it live and this
    /// frame traces it while published.
    pub actuals: *mut Value,
    /// Calling frame's record, or null for the outermost published frame.
    /// Linkage writes it before the frame becomes the innermost one.
    pub caller: u64,
    /// Stack-register frames in the chain from this record outward, this one
    /// included. The logical JavaScript depth of generated frames; linkage
    /// computes it from the caller's value.
    pub depth: u32,
    /// Safepoint of the optimized call this frame is making, or
    /// [`super::NO_SAFEPOINT`]. Optimized code names it before every
    /// collecting call; the collector resolves `(code_object_id, call_site)`
    /// to the tagged root homes at [`Self::machine_roots`].
    pub call_site: super::SafepointId,
    /// Base of the optimized frame's tagged root homes, written once by its
    /// prologue. Read only while `call_site` names a safepoint.
    pub machine_roots: u64,
    /// Destination in an interpreted caller, or `u32::MAX` for ABI return.
    pub return_destination: u32,
    /// Frame-local semantic state. Zero denotes an unallocated cold record.
    pub cold: Option<ColdFrameIdx>,
}

impl Frame {
    /// Trace the common frame's tagged fields and nullable arguments root.
    ///
    /// # Safety
    /// `frame` must remain live and initialized throughout this collector
    /// walk. The stopped mutator grants the visitor ownership of in-place
    /// relocation writes. Register windows are visited separately.
    pub(crate) unsafe fn trace_fields(frame: *mut Self, visitor: &mut SlotVisitor<'_>) {
        // SAFETY: the collector owns each short in-place slot rewrite. No
        // reference to the enclosing frame is retained across the callback.
        for slot in unsafe {
            [
                std::ptr::addr_of_mut!((*frame).self_value),
                std::ptr::addr_of_mut!((*frame).this_value),
                std::ptr::addr_of_mut!((*frame).new_target_value),
            ]
        } {
            unsafe { (&mut *slot).trace_value_slot_mut(visitor) };
        }
        let arguments = unsafe { std::ptr::addr_of_mut!((*frame).arguments_object) };
        if !unsafe { (*arguments).is_null() } {
            visitor(arguments.cast::<RawGc>());
        }
    }

    /// Address of this activation's tagged register window.
    pub fn register_base(&self) -> u64 {
        self.registers.as_mut_ptr() as u64
    }

    /// Interpreter caller destination, when the caller resumes bytecode.
    pub fn return_register(&self) -> Option<u16> {
        u16::try_from(self.return_destination).ok()
    }

    /// Select bytecode-register delivery or the generated return convention.
    pub fn set_return_register(&mut self, register: Option<u16>) {
        self.return_destination = register.map_or(u32::MAX, u32::from);
    }

    /// Construct the common state of a live native JS call.
    ///
    /// The caller retains the initialized register and actual windows until
    /// the activation is unpublished.
    #[must_use]
    pub fn new(
        header: VmFrameHeader,
        register_base: u64,
        self_value: Value,
        this_value: Value,
    ) -> Self {
        Self {
            header,
            code_object_id: 0,
            registers: RegisterWindow::attached(
                register_base as *mut Value,
                header.register_count as usize,
            ),
            this_value,
            new_target_value: Value::UNDEFINED,
            self_value,
            argument_count: 0,
            arguments_object: crate::object::JsObject::null(),
            actuals: std::ptr::null_mut(),
            caller: 0,
            depth: 0,
            call_site: super::NO_SAFEPOINT,
            machine_roots: 0,
            return_destination: u32::MAX,
            cold: None,
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
    pub fn caller_frame(&self) -> *mut Frame {
        self.caller as *mut Frame
    }

    /// Exact running function object.
    #[must_use]
    pub const fn self_value(&self) -> Value {
        self.self_value
    }

    /// Replace the exact running function object.
    pub fn set_self_value(&mut self, value: Value) {
        self.self_value = value;
    }

    /// Current `this` binding.
    #[must_use]
    pub const fn this_value(&self) -> Value {
        self.this_value
    }

    /// Replace the current `this` binding.
    pub fn set_this_value(&mut self, value: Value) {
        self.this_value = value;
    }

    /// Current `new.target` binding.
    #[must_use]
    pub const fn new_target(&self) -> Value {
        self.new_target_value
    }

    /// Replace the current `new.target` binding.
    pub fn set_new_target(&mut self, value: Value) {
        self.new_target_value = value;
    }

    /// Publish `count` initialized actual arguments at `actuals`.
    ///
    /// The owner keeps the span initialized and live while this frame is
    /// published; the frame traces it.
    pub fn set_incoming_arguments(&mut self, actuals: *mut Value, count: u32) {
        self.actuals = actuals;
        self.argument_count = count;
    }

    /// Number of initialized actual arguments at [`Self::actuals`].
    #[must_use]
    pub const fn incoming_argument_count(&self) -> u32 {
        self.argument_count
    }

    /// Mark an activation entered through `[[Construct]]`.
    pub fn set_construct(&mut self) {
        self.header.flags =
            NativeFrameFlags::from_bits(self.header.flags.bits() | NativeFrameFlags::CONSTRUCT);
    }

    /// Whether the caller requested constructor completion semantics.
    #[must_use]
    pub const fn is_construct(&self) -> bool {
        self.header.flags.contains(NativeFrameFlags::CONSTRUCT)
    }

    /// Mark this activation as a derived constructor.
    pub fn set_derived_constructor(&mut self) {
        self.header.flags = NativeFrameFlags::from_bits(
            self.header.flags.bits()
                | NativeFrameFlags::DERIVED_CONSTRUCTOR
                | NativeFrameFlags::CONSTRUCT,
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
    /// copying its register window. The header must describe the complete
    /// initialized extent; malformed metadata is rejected before any change.
    pub fn enter_interpreter(&mut self) -> bool {
        if usize::from(self.header.register_count) != self.registers.len() {
            return false;
        }
        self.header.kind = NativeFrameKind::Interpreter;
        self.header.flags = NativeFrameFlags::from_bits(
            self.header.flags.bits()
                & !(NativeFrameFlags::HAS_SAFEPOINTS | NativeFrameFlags::OSR_ENTRY),
        );
        self.code_object_id = 0;
        self.call_site = super::NO_SAFEPOINT;
        self.machine_roots = 0;
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

const _: [(); 96] = [(); std::mem::size_of::<VmThread>()];
const _: [(); 8] = [(); std::mem::align_of::<VmThread>()];
const _: [(); 12] = [(); std::mem::size_of::<VmFrameHeader>()];
const _: [(); 104] = [(); std::mem::size_of::<Frame>()];
const _: [(); 8] = [(); std::mem::align_of::<Frame>()];
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
const _: [(); 16] = [(); std::mem::offset_of!(Frame, registers)];
const _: [(); 32] = [(); std::mem::offset_of!(Frame, this_value)];
const _: [(); 40] = [(); std::mem::offset_of!(Frame, new_target_value)];
const _: [(); 48] = [(); std::mem::offset_of!(Frame, self_value)];
const _: [(); 56] = [(); std::mem::offset_of!(Frame, argument_count)];
const _: [(); 12] = [(); std::mem::offset_of!(Frame, code_object_id)];
const _: [(); 64] = [(); std::mem::offset_of!(Frame, actuals)];
const _: [(); 72] = [(); std::mem::offset_of!(Frame, caller)];
const _: [(); 80] = [(); std::mem::offset_of!(Frame, depth)];
const _: [(); 84] = [(); std::mem::offset_of!(Frame, call_site)];
const _: [(); 88] = [(); std::mem::offset_of!(Frame, machine_roots)];

/// Byte offset of [`Frame::code_object_id`]. It shares the eight-byte
/// word that starts at the register count, so linkage writes the register
/// shape, tier, flags and generation with one store.
pub const NATIVE_FRAME_CODE_OBJECT_ID_OFFSET: u32 =
    std::mem::offset_of!(Frame, code_object_id) as u32;
/// Byte offset of [`Frame::caller`].
pub const NATIVE_FRAME_CALLER_OFFSET: u32 = std::mem::offset_of!(Frame, caller) as u32;
/// Byte offset of [`Frame::depth`]. It shares an eight-byte word with
/// `call_site`, so linkage initializes both with one store.
pub const NATIVE_FRAME_DEPTH_OFFSET: u32 = std::mem::offset_of!(Frame, depth) as u32;
/// Byte offset of [`Frame::call_site`].
pub const NATIVE_FRAME_CALL_SITE_OFFSET: u32 = std::mem::offset_of!(Frame, call_site) as u32;
/// Byte offset of [`Frame::machine_roots`].
pub const NATIVE_FRAME_MACHINE_ROOTS_OFFSET: u32 =
    std::mem::offset_of!(Frame, machine_roots) as u32;

/// Byte offset of the header's initialized register count.
pub const NATIVE_FRAME_REGISTER_COUNT_OFFSET: u32 = (std::mem::offset_of!(Frame, header)
    + std::mem::offset_of!(VmFrameHeader, register_count))
    as u32;
/// Byte offset of [`Frame::register_base`].
pub const NATIVE_FRAME_REGISTER_BASE_OFFSET: u32 = std::mem::offset_of!(Frame, registers) as u32;
/// Register capacity followed by alignment padding initialized to zero.
pub const NATIVE_FRAME_REGISTER_EXTENT_OFFSET: u32 = NATIVE_FRAME_REGISTER_BASE_OFFSET + 8;
/// Return destination followed by the nullable cold-state index.
pub const NATIVE_FRAME_CONTINUATION_OFFSET: u32 =
    std::mem::offset_of!(Frame, return_destination) as u32;
/// Byte offset of [`Frame::this_value`].
pub const NATIVE_FRAME_THIS_OFFSET: u32 = std::mem::offset_of!(Frame, this_value) as u32;
/// Byte offset of [`Frame::new_target_value`].
pub const NATIVE_FRAME_NEW_TARGET_OFFSET: u32 =
    std::mem::offset_of!(Frame, new_target_value) as u32;
/// Byte offset of [`Frame::self_value`] (SELF). Generated
/// `LoadClosureContext` loads the closure here, then its context word.
pub const NATIVE_FRAME_SELF_OFFSET: u32 = std::mem::offset_of!(Frame, self_value) as u32;

/// Byte offset of [`Frame::argument_count`]. It shares an eight-byte word
/// with the nullable arguments object, so a prologue writes the count and a
/// null cache with one store.
pub const NATIVE_FRAME_ARGUMENT_COUNT_OFFSET: u32 =
    std::mem::offset_of!(Frame, argument_count) as u32;
/// Byte offset of [`Frame::actuals`].
pub const NATIVE_FRAME_ACTUALS_OFFSET: u32 = std::mem::offset_of!(Frame, actuals) as u32;

/// Nullable arguments object root; zero admits direct actual-window reads.
pub const NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET: u32 =
    std::mem::offset_of!(Frame, arguments_object) as u32;
const _: [(); 60] = [(); std::mem::offset_of!(Frame, arguments_object)];

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
    fn native_frame_publishes_an_actual_window_for_every_call() {
        let mut frame = Frame::new(
            VmFrameHeader::interpreter(7, 3),
            0x1000,
            Value::function(7),
            Value::number_i32(4),
        );
        assert_eq!(frame.self_value(), Value::function(7));
        assert_eq!(frame.this_value(), Value::number_i32(4));
        assert_eq!(frame.new_target(), Value::undefined());

        assert_eq!(frame.incoming_argument_count(), 0);
        let mut actuals = [Value::UNDEFINED; 5];
        frame.set_incoming_arguments(actuals.as_mut_ptr(), 5);
        assert_eq!(frame.incoming_argument_count(), 5);
        assert_eq!(frame.actuals, actuals.as_mut_ptr());
    }

    #[test]
    fn tier_switches_keep_one_native_activation_and_window() {
        let mut slots = [Value::number_i32(1), Value::number_i32(2)];
        let base = slots.as_mut_ptr() as u64;
        let mut frame = Frame::new(
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

        frame.set_construct();
        frame.code_object_id = 41;
        frame.header.register_count = 1;
        assert!(!frame.enter_interpreter());
        assert_eq!(frame.code_object_id, 41);
        assert_eq!(frame.header.kind, NativeFrameKind::Baseline);
        frame.header.register_count = 2;
        assert!(frame.enter_interpreter());
        assert!(frame.is_construct());
        assert_eq!(frame.code_object_id, 0);
        assert_eq!(frame.header.kind, NativeFrameKind::Interpreter);
        assert_eq!(frame.register_base(), base);
        assert_eq!(slots, [Value::number_i32(1), Value::number_i32(2)]);

        assert!(frame.enter_compiled(NativeFrameKind::Optimizing));
        assert_eq!(frame.header.kind, NativeFrameKind::Optimizing);
        assert_eq!(frame.register_base(), base);
        assert_eq!(slots, [Value::number_i32(1), Value::number_i32(2)]);
    }

    #[test]
    fn native_frame_layout_holds_no_binding_storage() {
        assert_eq!(std::mem::size_of::<VmFrameHeader>(), 12);
        assert_eq!(std::mem::size_of::<Frame>(), 104);
        assert_eq!(NATIVE_FRAME_REGISTER_BASE_OFFSET, 16);
        assert_eq!(NATIVE_FRAME_THIS_OFFSET, 32);
        assert_eq!(NATIVE_FRAME_NEW_TARGET_OFFSET, 40);
        assert_eq!(NATIVE_FRAME_SELF_OFFSET, 48);
        assert_eq!(NATIVE_FRAME_ARGUMENT_COUNT_OFFSET, 56);
        assert_eq!(NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET, 60);
        assert_eq!(NATIVE_FRAME_ACTUALS_OFFSET, 64);
        assert_eq!(NATIVE_FRAME_CALLER_OFFSET, 72);
        assert_eq!(NATIVE_FRAME_DEPTH_OFFSET, 80);
        assert_eq!(NATIVE_FRAME_CALL_SITE_OFFSET, 84);
        assert_eq!(NATIVE_FRAME_MACHINE_ROOTS_OFFSET, 88);
    }
}
