//! Direct access to the immutable shape-selected bank of an own field.
//!
//! # Contents
//! - [`emit`] loads or stores one field with the VM's header-inclusive layout.
//! - Native probes cover both banks and immediate-addressing boundaries.
//!
//! # Invariants
//! - The caller proves the receiver shape and owns the store's write barrier.
//! - No storage address becomes an SSA value or survives a collecting call.
//! - Inline access needs one instruction; suffix access rereads its live handle.
//! - Only the caller's value register and reserved x16/x17 may be overwritten.
//!
//! # See also
//! - `otter_vm::object::FieldLocation` for bank and relative index ownership.
//! - [`super::Codegen`] for shape guards, canonical homes and GC boundaries.

use dynasmrt::{DynasmApi, aarch64::Assembler, dynasm};
use otter_vm::object::{FieldLayout, FieldLocation};

pub(super) fn emit(
    ops: &mut Assembler,
    layout: FieldLayout,
    object: u8,
    value: u8,
    field: FieldLocation,
    store: bool,
) {
    let (base, offset) = if field.is_inline() {
        (object, layout.inline_byte(field))
    } else {
        dynasm!(ops ; .arch aarch64
            ; ldr w17, [X(object), layout.slab_handle_byte]
            ; and x16, X(object), #0xffff_ffff_0000_0000
            ; add x17, x16, x17);
        (
            17,
            layout
                .slab_words_byte
                .checked_add(field.byte_offset())
                .expect("field offset fits VM layout"),
        )
    };
    if offset <= 32760 && offset % 8 == 0 {
        if store {
            dynasm!(ops ; .arch aarch64 ; str X(value), [X(base), offset]);
        } else {
            dynasm!(ops ; .arch aarch64 ; ldr X(value), [X(base), offset]);
        }
    } else {
        crate::template::arm64::values::emit_load_u64(ops, 16, u64::from(offset));
        if store {
            dynasm!(ops ; .arch aarch64 ; str X(value), [X(base), x16]);
        } else {
            dynasm!(ops ; .arch aarch64 ; ldr X(value), [X(base), x16]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynasmrt::{AssemblyOffset, DynasmApi};

    #[test]
    fn own_field_native_access_uses_the_live_bank_without_an_ssa_storage_pointer() {
        let layout = FieldLayout::current();
        let mut words = vec![0u64; 6000];
        let object = words.as_mut_ptr() as u64;
        let slab = unsafe { words.as_mut_ptr().add(80) } as u64;
        assert_eq!(
            object >> 32,
            slab >> 32,
            "probe storage shares one cage base"
        );
        // SAFETY: the probe owns both aligned raw cells. Only the slab handle
        // and the selected field words are read by generated code; no GC runs.
        unsafe {
            ((object + u64::from(layout.slab_handle_byte)) as *mut u32).write(slab as u32);
        }
        for field in [
            FieldLocation::inline(0),
            FieldLocation::inline(63),
            FieldLocation::overflow(0),
            FieldLocation::overflow(63),
            FieldLocation::overflow(5000),
        ] {
            let mut ops = Assembler::new().unwrap();
            emit(&mut ops, layout, 0, 1, field, true);
            let store_end = ops.offset().0;
            emit(&mut ops, layout, 0, 0, field, false);
            let load_end = ops.offset().0;
            dynasm!(ops ; .arch aarch64 ; ret);
            let buffer = ops.finalize().unwrap();
            // SAFETY: buffer contains a two-word native probe with no call or
            // collecting boundary; its argument points into the owned storage.
            let probe: unsafe extern "C" fn(u64, u64) -> u64 =
                unsafe { std::mem::transmute(buffer.ptr(AssemblyOffset(0))) };
            let expected = 0x1234_5678_abcd_ef01;
            assert_eq!(unsafe { probe(object, expected) }, expected, "{field:?}");
            let address = if field.is_inline() {
                object + u64::from(layout.inline_byte(field))
            } else {
                slab + u64::from(layout.slab_words_byte + field.byte_offset())
            };
            assert_eq!(unsafe { *(address as *const u64) }, expected);
            if field.is_inline() {
                assert_eq!(store_end, 4, "inline store is one instruction");
                assert_eq!(load_end - store_end, 4, "inline load is one instruction");
            } else if field.index() < 4090 {
                assert_eq!(store_end, 16, "suffix resolves its handle then stores");
                assert_eq!(
                    load_end - store_end,
                    16,
                    "suffix resolves its handle then loads"
                );
            }
        }
    }
}
