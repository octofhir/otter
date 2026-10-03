//! AArch64 indexed element access on allocated registers.
//!
//! # Contents
//! - [`emit_view`] / [`emit_proof`] — prove an indexed receiver and produce
//!   its element base and live length, or only its length for a dense
//!   receiver whose base is read at the access.
//! - [`emit_address`] — the committed form's index and bounds proof.
//! - [`emit_value_load`] / [`emit_value_guard`] — the committed form's read
//!   and store-admission over a proved address.
//! - [`emit_checked`] — the speculative form's single proof-and-read (or
//!   proof-and-address) that exits on any failed proof.
//! - [`emit_value_store`] — the no-fail write after a checked address.
//!
//! # Invariants
//! - Every operand and result is in the register the allocator assigned.
//!   The only scratch is `x15`–`x17`, `d30` and `d31`, all outside the
//!   allocation file, so no element operation clobbers an allocatable
//!   register.
//! - A result may share a register with an input that dies at the
//!   operation: every path reads its inputs before it writes a result, and a
//!   miss writes only the boolean results (a tagged payload reads
//!   `undefined`).
//! - A cell value is its header's full address; only compressed body handles
//!   (a view's buffer) are decompressed against the cage base.
//! - A fixed view's cached base stands for the full buffer proof while no
//!   buffer has ever been detached; otherwise the live buffer is proved.
//!
//! # See also
//! - `crate::template::arm64::ic_probe` — the baseline tier's element probes.
//! - `super::super::super::element` — the representation contract.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{
    JitBodyGuard, JitElementAccess, JitElementBase, JitElementRepr, JitGuardWidth,
};

use super::super::super::MachineRepresentation;
use super::NUMBER_TAG;
use crate::Unsupported;
use crate::artifact::relocation::{RelocationCapture, RelocationTarget};
use crate::entry::{
    CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, THREAD_OFFSET, VALUE_HOLE, VALUE_UNDEFINED,
};
use crate::template::arm64::values::{emit_load_symbol_u64, emit_load_u64};

const DETACH_PROTECTOR: u32 = crate::entry::VM_THREAD_ARRAY_BUFFER_DETACH_PROTECTOR_CELL_OFFSET;
const NOT_CELL_MASK: u64 = otter_vm::value::tag::NOT_CELL_MASK;

/// One allocated index operand.
#[derive(Clone, Copy)]
pub(super) struct Index {
    pub(super) register: u8,
    pub(super) representation: MachineRepresentation,
}

/// One allocated element value: an integer register holding a tagged,
/// int32 or uint32 value, or a floating-point register.
#[derive(Clone, Copy)]
pub(super) struct Element {
    pub(super) register: u8,
    pub(super) representation: MachineRepresentation,
}

/// The base of a checked access: a raw element base, or the receiver of a
/// dense access whose live base is read at the access.
#[derive(Clone, Copy)]
pub(super) enum Base {
    Raw(u8),
    Receiver(u8),
}

fn emit_body_guard(ops: &mut Assembler, receiver: u8, guard: JitBodyGuard, miss: DynamicLabel) {
    let byte = guard.byte;
    match guard.width {
        JitGuardWidth::Byte => dynasm!(ops ; .arch aarch64 ; ldrb w17, [X(receiver), byte]),
        JitGuardWidth::Word32 => dynasm!(ops ; .arch aarch64 ; ldr w17, [X(receiver), byte]),
        JitGuardWidth::Word64 => dynasm!(ops ; .arch aarch64 ; ldr x17, [X(receiver), byte]),
    }
    if guard.expect == 0 {
        dynasm!(ops ; .arch aarch64 ; cbnz x17, =>miss);
    } else if guard.expect < 4096 {
        dynasm!(ops ; .arch aarch64 ; cmp x17, guard.expect ; b.ne =>miss);
    } else {
        emit_load_u64(ops, 16, u64::from(guard.expect));
        dynasm!(ops ; .arch aarch64 ; cmp x17, x16 ; b.ne =>miss);
    }
}

/// Prove `X(receiver)` a heap cell of the access's body type satisfying its
/// body guards, and load its live length into `x15`.
fn emit_receiver_proof(
    ops: &mut Assembler,
    access: &JitElementAccess,
    receiver: u8,
    miss: DynamicLabel,
) {
    emit_load_u64(ops, 16, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch aarch64
        ; tst X(receiver), x16
        ; b.ne =>miss
        ; ldrb w17, [X(receiver)]
        ; cmp w17, access.type_tag as u32
        ; b.ne =>miss
    );
    for guard in access.guards.iter().flatten() {
        emit_body_guard(ops, receiver, *guard, miss);
    }
    let length = access.length_byte;
    match access.length_width {
        JitGuardWidth::Byte => dynasm!(ops ; .arch aarch64 ; ldrb w15, [X(receiver), length]),
        JitGuardWidth::Word32 => dynasm!(ops ; .arch aarch64 ; ldr w15, [X(receiver), length]),
        JitGuardWidth::Word64 => dynasm!(ops ; .arch aarch64 ; ldr x15, [X(receiver), length]),
    }
}

/// `ElementView`: on success `X(base)` is the element base, `X(length)` the
/// zero-extended live element count and `W(hit)` is one; on a miss only
/// `W(hit)` is written, as zero.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_view(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    access: &JitElementAccess,
    receiver: u8,
    [base, length, hit]: [u8; 3],
) -> Result<(), Unsupported> {
    let miss = ops.new_dynamic_label();
    let ready = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_receiver_proof(ops, access, receiver, miss);
    match access.base {
        JitElementBase::None => return Err(Unsupported::OperandShape("element view base")),
        JitElementBase::InBody { byte } => {
            dynasm!(ops ; .arch aarch64 ; ldr x16, [X(receiver), byte] ; b =>ready);
        }
        JitElementBase::ThroughLocalBuffer {
            storage_tag_byte,
            local_tag,
            handle_byte,
            detached_byte,
            data_ptr_byte,
            byte_len_byte,
            view_offset_byte,
            cached_data_byte,
        } => {
            let slow = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch aarch64
                ; ldr x16, [X(receiver), cached_data_byte]
                ; cbz x16, =>slow
                ; ldr x17, [x19, THREAD_OFFSET]
                ; ldr x17, [x17, DETACH_PROTECTOR]
                ; cbz x17, =>slow
                ; ldrb w17, [x17]
                ; cbz w17, =>ready
                ; =>slow
                ; ldr w17, [X(receiver), storage_tag_byte]
            );
            emit_load_u64(ops, 16, u64::from(local_tag));
            dynasm!(ops
                ; .arch aarch64
                ; cmp w17, w16
                ; b.ne =>miss
                ; ldr w16, [X(receiver), handle_byte]
                ; cbz w16, =>miss
            );
            emit_load_symbol_u64(
                ops,
                relocations,
                17,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            // x16: the live buffer header; x17: the view's byte offset. The
            // receiver is dead after the offset read, so the results serve
            // as scratch from here on.
            dynasm!(ops
                ; .arch aarch64
                ; add x16, x17, x16
                ; ldrb w17, [x16, detached_byte]
                ; cbnz w17, =>miss
                ; ldr x17, [X(receiver), view_offset_byte]
            );
            // Byte extent of the view, rejecting one that overflows 64 bits.
            let shift = access.element.stride_shift();
            if shift == 0 {
                dynasm!(ops ; .arch aarch64 ; mov X(hit), x15);
            } else {
                dynasm!(ops
                    ; .arch aarch64
                    ; lsr X(hit), x15, 64 - shift
                    ; cbnz X(hit), =>miss
                    ; lsl X(hit), x15, shift
                );
            }
            dynasm!(ops
                ; .arch aarch64
                ; adds X(hit), x17, X(hit)
                ; b.cs =>miss
                ; ldr X(base), [x16, byte_len_byte]
                ; cmp X(hit), X(base)
                ; b.hi =>miss
                ; ldr X(base), [x16, data_ptr_byte]
                ; cbz X(base), =>miss
                ; add x16, X(base), x17
            );
        }
    }
    dynasm!(ops
        ; .arch aarch64
        ; =>ready
        ; mov X(base), x16
        ; mov X(length), x15
        ; mov W(hit), #1
        ; b =>done
        ; =>miss
        ; mov W(hit), wzr
        ; =>done
    );
    Ok(())
}

/// `ElementProof`: a dense receiver's live length and a hit bit. The access
/// reads the receiver's live base itself.
pub(super) fn emit_proof(
    ops: &mut Assembler,
    access: &JitElementAccess,
    receiver: u8,
    [length, hit]: [u8; 2],
) -> Result<(), Unsupported> {
    if !matches!(access.base, JitElementBase::InBody { .. }) {
        return Err(Unsupported::OperandShape("dense element proof base"));
    }
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    emit_receiver_proof(ops, access, receiver, miss);
    dynasm!(ops
        ; .arch aarch64
        ; mov X(length), x15
        ; mov W(hit), #1
        ; b =>done
        ; =>miss
        ; mov W(hit), wzr
        ; =>done
    );
    Ok(())
}

/// Leave the index in `x17`, extended to 64 bits so one unsigned compare
/// against the length also rejects a negative int32.
fn emit_index(ops: &mut Assembler, index: Index, miss: DynamicLabel) -> Result<(), Unsupported> {
    let register = index.register;
    match index.representation {
        MachineRepresentation::Int32 => dynasm!(ops ; .arch aarch64 ; sxtw x17, W(register)),
        MachineRepresentation::Uint32 => dynasm!(ops ; .arch aarch64 ; mov w17, W(register)),
        MachineRepresentation::Tagged => dynasm!(ops
            ; .arch aarch64
            // An int32 box carries 0xfffe in its top half-word.
            ; asr x17, X(register), #48
            ; cmn x17, #2
            ; b.ne =>miss
            ; sxtw x17, W(register)
        ),
        MachineRepresentation::Float64 => dynasm!(ops
            ; .arch aarch64
            ; fcvtzu w17, D(register)
            ; ucvtf d31, w17
            ; fcmp d31, D(register)
            ; b.ne =>miss
        ),
        _ => return Err(Unsupported::OperandShape("element index representation")),
    }
    Ok(())
}

/// Box the double in `d31` into `X(target)` exactly as the arithmetic paths
/// do: an integral in-range non-`-0` value as an int32, anything else as a
/// canonical double. Clobbers `x15`, `x16`, `d30`.
fn emit_box_number(ops: &mut Assembler, target: u8) {
    let double = ops.new_dynamic_label();
    let int = ops.new_dynamic_label();
    let ordered = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; fcvtzs w15, d31
        ; scvtf d30, w15
        ; fcmp d31, d30
        ; b.ne =>double
        ; cbnz w15, =>int
        ; fmov x16, d31
        ; tbnz x16, #63, =>double
        ; =>int
        ; mov W(target), w15
        ; orr XSP(target), X(target), NUMBER_TAG
        ; b =>done
        ; =>double
        ; fmov X(target), d31
        ; fcmp d31, d31
        ; b.vc =>ordered
        ; movz X(target), CANONICAL_NAN_HI16, lsl #48
        ; =>ordered
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; add X(target), X(target), x16
        ; =>done
    );
}

/// Decode the number in `X(source)` into `d31`. The caller already proved it
/// a number. Clobbers `x16`.
fn emit_number_to_double(ops: &mut Assembler, source: u8) {
    let double = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; asr x16, X(source), #48
        ; cmn x16, #2
        ; b.ne =>double
        ; scvtf d31, W(source)
        ; b =>done
        ; =>double
        ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
        ; sub x16, X(source), x16
        ; fmov d31, x16
        ; =>done
    );
}

/// Read the element at `[X(base) + x17 << stride]` into `result`. A boxed
/// hole branches to `miss`. Clobbers `x15`, `x16`, `d30`, `d31`.
fn emit_read(
    ops: &mut Assembler,
    element: JitElementRepr,
    base: u8,
    result: Element,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    use JitElementRepr as E;
    let out = result.register;
    match result.representation {
        MachineRepresentation::Int32 | MachineRepresentation::Uint32 => match element {
            E::Int8 => dynasm!(ops ; .arch aarch64 ; ldrsb W(out), [X(base), x17]),
            E::Uint8 | E::Uint8Clamped => {
                dynasm!(ops ; .arch aarch64 ; ldrb W(out), [X(base), x17])
            }
            E::Int16 => dynasm!(ops ; .arch aarch64 ; ldrsh W(out), [X(base), x17, lsl #1]),
            E::Uint16 => dynasm!(ops ; .arch aarch64 ; ldrh W(out), [X(base), x17, lsl #1]),
            E::Int32 | E::Uint32 => {
                dynasm!(ops ; .arch aarch64 ; ldr W(out), [X(base), x17, lsl #2])
            }
            _ => {
                return Err(Unsupported::OperandShape(
                    "scalar element load representation",
                ));
            }
        },
        MachineRepresentation::Float64 => match element {
            E::Float32 => dynasm!(ops
                ; .arch aarch64
                ; ldr s31, [X(base), x17, lsl #2]
                ; fcvt D(out), s31
            ),
            E::Float64 => dynasm!(ops ; .arch aarch64 ; ldr D(out), [X(base), x17, lsl #3]),
            _ => {
                return Err(Unsupported::OperandShape(
                    "floating element load representation",
                ));
            }
        },
        MachineRepresentation::Tagged => match element {
            E::Boxed => {
                dynasm!(ops ; .arch aarch64 ; ldr x15, [X(base), x17, lsl #3]);
                emit_load_u64(ops, 16, VALUE_HOLE);
                dynasm!(ops
                    ; .arch aarch64
                    ; cmp x15, x16
                    ; b.eq =>miss
                    ; mov X(out), x15
                );
            }
            E::Int8 | E::Uint8 | E::Uint8Clamped | E::Int16 | E::Uint16 | E::Int32 => {
                match element {
                    E::Int8 => dynasm!(ops ; .arch aarch64 ; ldrsb W(out), [X(base), x17]),
                    E::Uint8 | E::Uint8Clamped => {
                        dynasm!(ops ; .arch aarch64 ; ldrb W(out), [X(base), x17])
                    }
                    E::Int16 => dynasm!(ops ; .arch aarch64 ; ldrsh W(out), [X(base), x17, lsl #1]),
                    E::Uint16 => dynasm!(ops ; .arch aarch64 ; ldrh W(out), [X(base), x17, lsl #1]),
                    _ => dynasm!(ops ; .arch aarch64 ; ldr W(out), [X(base), x17, lsl #2]),
                }
                // A W write cleared the upper half: one OR completes the box.
                dynasm!(ops ; .arch aarch64 ; orr XSP(out), X(out), NUMBER_TAG);
            }
            E::Uint32 => {
                let double = ops.new_dynamic_label();
                let done = ops.new_dynamic_label();
                dynasm!(ops
                    ; .arch aarch64
                    ; ldr w15, [X(base), x17, lsl #2]
                    ; tst w15, 0x8000_0000
                    ; b.ne =>double
                    ; orr XSP(out), x15, NUMBER_TAG
                    ; b =>done
                    ; =>double
                    ; ucvtf d31, w15
                    ; fmov X(out), d31
                    ; movz x16, DOUBLE_OFFSET_HI16, lsl #48
                    ; add X(out), X(out), x16
                    ; =>done
                );
            }
            E::Float32 => {
                dynasm!(ops ; .arch aarch64 ; ldr s31, [X(base), x17, lsl #2] ; fcvt d31, s31);
                emit_box_number(ops, out);
            }
            E::Float64 => {
                dynasm!(ops ; .arch aarch64 ; ldr d31, [X(base), x17, lsl #3]);
                emit_box_number(ops, out);
            }
        },
        _ => return Err(Unsupported::OperandShape("element load representation")),
    }
    Ok(())
}

/// Branch to `miss` unless the stored value is directly storable in
/// `element`: a value already in the element's representation stores
/// exactly, and a boxed element takes any non-cell (a cell owes the
/// generational barrier only the runtime runs). Clobbers `x17`.
fn emit_write_guard(
    ops: &mut Assembler,
    element: JitElementRepr,
    stored: Element,
    miss: DynamicLabel,
) {
    if stored.representation != MachineRepresentation::Tagged {
        return;
    }
    let value = stored.register;
    match element {
        JitElementRepr::Boxed => {
            emit_load_u64(ops, 17, NOT_CELL_MASK);
            dynasm!(ops ; .arch aarch64 ; tst X(value), x17 ; b.eq =>miss);
        }
        JitElementRepr::Float32 | JitElementRepr::Float64 => {
            dynasm!(ops ; .arch aarch64 ; tst X(value), NUMBER_TAG ; b.eq =>miss);
        }
        _ => dynasm!(ops
            ; .arch aarch64
            ; asr x17, X(value), #48
            ; cmn x17, #2
            ; b.ne =>miss
        ),
    }
}

/// The boxed-element hole proof at `[x16]`: an absent slot leaves the fast
/// path, because a write would have to consult the prototype chain.
/// Clobbers `x15`, `x17`.
fn emit_present_slot(ops: &mut Assembler, element: JitElementRepr, miss: DynamicLabel) {
    if element == JitElementRepr::Boxed {
        dynasm!(ops ; .arch aarch64 ; ldr x15, [x16]);
        emit_load_u64(ops, 17, VALUE_HOLE);
        dynasm!(ops ; .arch aarch64 ; cmp x15, x17 ; b.eq =>miss);
    }
}

/// `ElementAddress`: the committed form's index and bounds proof over a
/// proved view. On success `X(address)` is the element address.
pub(super) fn emit_address(
    ops: &mut Assembler,
    access: &JitElementAccess,
    [base, length, active]: [u8; 3],
    index: Index,
    [address, hit]: [u8; 2],
) -> Result<(), Unsupported> {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let shift = access.element.stride_shift();
    dynasm!(ops ; .arch aarch64 ; cbz W(active), =>miss);
    emit_index(ops, index, miss)?;
    dynasm!(ops
        ; .arch aarch64
        ; cmp x17, X(length)
        ; b.hs =>miss
        ; add X(address), X(base), x17, lsl shift
        ; mov W(hit), #1
        ; b =>done
        ; =>miss
        ; mov W(hit), wzr
        ; =>done
    );
    Ok(())
}

/// `ElementValueLoad`: read a proved address. A miss reads `undefined`.
pub(super) fn emit_value_load(
    ops: &mut Assembler,
    access: &JitElementAccess,
    [address, active]: [u8; 2],
    result: Element,
    hit: u8,
) -> Result<(), Unsupported> {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; cbz W(active), =>miss
        ; mov x16, X(address)
        ; mov x17, xzr
    );
    emit_read(ops, access.element, 16, result, miss)?;
    dynasm!(ops ; .arch aarch64 ; mov W(hit), #1 ; b =>done ; =>miss);
    if result.representation == MachineRepresentation::Tagged {
        emit_load_u64(ops, result.register, VALUE_UNDEFINED);
    }
    dynasm!(ops ; .arch aarch64 ; mov W(hit), wzr ; =>done);
    Ok(())
}

/// `ElementValueGuard`: admit a store at a proved address.
pub(super) fn emit_value_guard(
    ops: &mut Assembler,
    access: &JitElementAccess,
    [address, active]: [u8; 2],
    stored: Element,
    hit: u8,
) {
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; cbz W(active), =>miss ; mov x16, X(address));
    emit_present_slot(ops, access.element, miss);
    emit_write_guard(ops, access.element, stored, miss);
    dynasm!(ops
        ; .arch aarch64
        ; mov W(hit), #1
        ; b =>done
        ; =>miss
        ; mov W(hit), wzr
        ; =>done
    );
}

/// `ElementCheckedLoad` / `ElementCheckedAddress`: prove the view's hit, the
/// index, the bounds and the slot (or the stored value) and exit to `deopt`
/// on any failure; then read the element into `result`, or leave its address
/// in `X(address)`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_checked(
    ops: &mut Assembler,
    access: &JitElementAccess,
    base: Base,
    [length, active]: [u8; 2],
    index: Index,
    access_result: CheckedResult,
    deopt: DynamicLabel,
) -> Result<(), Unsupported> {
    dynasm!(ops ; .arch aarch64 ; cbz W(active), =>deopt);
    let base = match base {
        Base::Raw(register) => register,
        Base::Receiver(receiver) => {
            let JitElementBase::InBody { byte } = access.base else {
                return Err(Unsupported::OperandShape("dense element base"));
            };
            dynasm!(ops ; .arch aarch64 ; ldr x16, [X(receiver), byte]);
            16
        }
    };
    emit_index(ops, index, deopt)?;
    dynasm!(ops ; .arch aarch64 ; cmp x17, X(length) ; b.hs =>deopt);
    match access_result {
        CheckedResult::Load(result) => emit_read(ops, access.element, base, result, deopt),
        CheckedResult::Address { stored, address } => {
            let shift = access.element.stride_shift();
            dynasm!(ops ; .arch aarch64 ; add x16, X(base), x17, lsl shift);
            emit_present_slot(ops, access.element, deopt);
            emit_write_guard(ops, access.element, stored, deopt);
            dynasm!(ops ; .arch aarch64 ; mov X(address), x16);
            Ok(())
        }
    }
}

/// What a checked access produces.
#[derive(Clone, Copy)]
pub(super) enum CheckedResult {
    Load(Element),
    Address { stored: Element, address: u8 },
}

/// `ElementValueStore`: write `value` at a checked address in `element`'s
/// representation. The checked address already admitted the value.
pub(super) fn emit_value_store(
    ops: &mut Assembler,
    element: JitElementRepr,
    address: u8,
    value: Element,
) -> Result<(), Unsupported> {
    use JitElementRepr as E;
    let source = value.register;
    match (element, value.representation) {
        (E::Boxed, MachineRepresentation::Tagged) => {
            dynasm!(ops ; .arch aarch64 ; str X(source), [X(address)]);
        }
        (E::Boxed, MachineRepresentation::Int32) => dynasm!(ops
            ; .arch aarch64
            ; mov w16, W(source)
            ; orr x16, x16, NUMBER_TAG
            ; str x16, [X(address)]
        ),
        (E::Boxed, MachineRepresentation::Float64) => {
            dynasm!(ops ; .arch aarch64 ; fmov d31, D(source));
            emit_box_number(ops, 17);
            dynasm!(ops ; .arch aarch64 ; str x17, [X(address)]);
        }
        (
            E::Int8 | E::Uint8,
            MachineRepresentation::Tagged
            | MachineRepresentation::Int32
            | MachineRepresentation::Uint32,
        ) => dynasm!(ops ; .arch aarch64 ; strb W(source), [X(address)]),
        (
            E::Int16 | E::Uint16,
            MachineRepresentation::Tagged
            | MachineRepresentation::Int32
            | MachineRepresentation::Uint32,
        ) => dynasm!(ops ; .arch aarch64 ; strh W(source), [X(address)]),
        (
            E::Int32 | E::Uint32,
            MachineRepresentation::Tagged
            | MachineRepresentation::Int32
            | MachineRepresentation::Uint32,
        ) => dynasm!(ops ; .arch aarch64 ; str W(source), [X(address)]),
        (E::Uint8Clamped, MachineRepresentation::Tagged | MachineRepresentation::Int32) => {
            dynasm!(ops
                ; .arch aarch64
                ; cmp WSP(source), #0
                ; csel w16, wzr, W(source), lt
                ; mov w17, #255
                ; cmp w16, w17
                ; csel w16, w17, w16, gt
                ; strb w16, [X(address)]
            );
        }
        (E::Float32, MachineRepresentation::Float64) => dynasm!(ops
            ; .arch aarch64
            ; fcvt s31, D(source)
            ; str s31, [X(address)]
        ),
        (E::Float64, MachineRepresentation::Float64) => {
            dynasm!(ops ; .arch aarch64 ; str D(source), [X(address)]);
        }
        (E::Float32, MachineRepresentation::Int32) => dynasm!(ops
            ; .arch aarch64
            ; scvtf s31, W(source)
            ; str s31, [X(address)]
        ),
        (E::Float64, MachineRepresentation::Int32) => dynasm!(ops
            ; .arch aarch64
            ; scvtf d31, W(source)
            ; str d31, [X(address)]
        ),
        (E::Float32, MachineRepresentation::Tagged) => {
            emit_number_to_double(ops, source);
            dynasm!(ops ; .arch aarch64 ; fcvt s31, d31 ; str s31, [X(address)]);
        }
        (E::Float64, MachineRepresentation::Tagged) => {
            emit_number_to_double(ops, source);
            dynasm!(ops ; .arch aarch64 ; str d31, [X(address)]);
        }
        _ => return Err(Unsupported::OperandShape("element store representation")),
    }
    Ok(())
}
