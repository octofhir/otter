//! Linker ownership and exact native-entry span proofs.
//!
//! # Contents
//! - Ordered coordinates inside each retained entry body.
//! - Actual linked Mach-O text/symbol coordinates and ARM branch ownership.
//!
//! # Invariants
//! - Referencing a coordinate can keep an otherwise stripped atom alive; span
//!   tests therefore do not substitute for the unchanged default-off native
//!   getter regression. Input-object alternate-entry flags and default-off
//!   production retention are separate linker evidence. Executables do not
//!   preserve the input-only `N_ALT_ENTRY` flag.
//! - Parsing reads the current executable as owned bytes and invokes no VM code.
//!
//! # See also
//! - `array::dense_guard::tests` exercises the ordinary collecting Host getter.

use super::*;

#[test]
fn native_entry_coordinates_cover_the_retained_instruction_bodies() {
    let generic = super::super::call_generic_entry as *const () as usize;
    let native = super::super::call_native_entry as *const () as usize;
    let publisher = super::super::call_request_entry as *const () as usize;
    let trampoline = super::super::call_trampoline as *const () as usize;
    let generic_end = std::ptr::addr_of!(GENERIC_END).addr();
    let native_end = std::ptr::addr_of!(NATIVE_END).addr();
    let publisher_end = std::ptr::addr_of!(PUBLISHER_END).addr();
    let header_start = std::ptr::addr_of!(HEADER_START).addr();
    let header_end = std::ptr::addr_of!(HEADER_END).addr();
    let classifier = std::ptr::addr_of!(CLASSIFIER).addr();
    let reservation = std::ptr::addr_of!(RESERVATION).addr();
    let invoke = std::ptr::addr_of!(INVOKE).addr();
    let trampoline_end = std::ptr::addr_of!(TRAMPOLINE_END).addr();

    assert!(generic < generic_end);
    assert!(native < native_end);
    assert!(publisher < header_start);
    assert!(header_start < header_end);
    assert!(header_end < publisher_end);
    assert!(trampoline < classifier);
    assert!(classifier < reservation);
    assert!(reservation < invoke);
    assert!(invoke < trampoline_end);

    let sizes = native_entry_code_sizes();
    assert_eq!(sizes.generic_selector_bytes as usize, generic_end - generic);
    assert_eq!(sizes.native_selector_bytes as usize, native_end - native);
    assert_eq!(
        sizes.request_publisher_bytes as usize,
        publisher_end - publisher
    );
    assert_eq!(
        sizes.native_header_bytes as usize,
        header_end - header_start
    );
    assert_eq!(sizes.trampoline_bytes as usize, trampoline_end - trampoline);
    assert_eq!(sizes.classifier_bytes as usize, reservation - classifier);
    assert_eq!(sizes.reservation_bytes as usize, invoke - reservation);
}

#[cfg(target_vendor = "apple")]
#[test]
fn macho_linked_coordinates_cover_the_owned_native_control_flow() {
    use std::collections::BTreeMap;

    const SYMBOLS: [&str; 9] = [
        "_otter_native_generic_end",
        "_otter_native_selected_end",
        "_otter_native_publisher_end",
        "_otter_native_header_start",
        "_otter_native_header_end",
        "_otter_native_classifier",
        "_otter_native_reservation",
        "_otter_native_invoke",
        "_otter_native_trampoline_end",
    ];

    fn word(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }
    fn double_word(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    let executable = std::env::current_exe().expect("current native test executable");
    let bytes = std::fs::read(&executable).expect("read actual Mach-O executable");
    assert!(bytes.len() >= 32, "complete Mach-O header");
    assert_eq!(
        word(&bytes, 0),
        0xfeed_facf,
        "native little-endian Mach-O64"
    );
    assert_eq!(word(&bytes, 12), 2, "actual linked MH_EXECUTE");
    let command_count = word(&bytes, 16) as usize;
    let commands_end = 32usize.checked_add(word(&bytes, 20) as usize).unwrap();
    assert!(commands_end <= bytes.len());
    let mut offset = 32;
    let mut section_number = 0u32;
    let mut text = None;
    let mut symtab = None;
    for _ in 0..command_count {
        assert!(offset + 8 <= commands_end);
        let command = word(&bytes, offset);
        let size = word(&bytes, offset + 4) as usize;
        assert!(size >= 8 && size <= commands_end - offset);
        if command == 0x19 {
            assert!(size >= 72);
            let sections = word(&bytes, offset + 64) as usize;
            assert!(sections <= (size - 72) / 80);
            for index in 0..sections {
                section_number += 1;
                let section = offset + 72 + index * 80;
                if &bytes[section..section + 16] != b"__text\0\0\0\0\0\0\0\0\0\0"
                    || &bytes[section + 16..section + 32] != b"__TEXT\0\0\0\0\0\0\0\0\0\0"
                {
                    continue;
                }
                assert!(text.is_none(), "one native text owner");
                let address = double_word(&bytes, section + 32);
                let length = usize::try_from(double_word(&bytes, section + 40)).unwrap();
                let file_offset = word(&bytes, section + 48) as usize;
                let end = file_offset.checked_add(length).unwrap();
                assert!(end <= bytes.len());
                text = Some((section_number, address, &bytes[file_offset..end]));
            }
        } else if command == 2 {
            assert!(size >= 24);
            assert!(symtab.is_none(), "one LC_SYMTAB owner");
            symtab = Some((
                word(&bytes, offset + 8) as usize,
                word(&bytes, offset + 12) as usize,
                word(&bytes, offset + 16) as usize,
                word(&bytes, offset + 20) as usize,
            ));
        }
        offset += size;
    }
    assert_eq!(offset, commands_end);
    let (text_section, text_address, text_bytes) = text.expect("actual linked native text");
    let (symbols_offset, symbol_count, strings_offset, strings_size) =
        symtab.expect("actual executable symbol table; stripped evidence is a gap");
    let symbols_end = symbols_offset
        .checked_add(symbol_count.checked_mul(16).unwrap())
        .unwrap();
    let strings_end = strings_offset.checked_add(strings_size).unwrap();
    assert!(symbols_end <= bytes.len() && strings_end <= bytes.len());
    let strings = &bytes[strings_offset..strings_end];
    let mut values = BTreeMap::new();
    for index in 0..symbol_count {
        let entry = symbols_offset + index * 16;
        let kind = bytes[entry + 4];
        // STAB records may repeat the same spelling without linker ownership.
        if kind & 0xe0 != 0 || kind & 0x0e != 0x0e {
            continue;
        }
        let string_index = word(&bytes, entry) as usize;
        assert!(string_index < strings.len());
        let name = &strings[string_index..];
        let end = name.iter().position(|byte| *byte == 0).unwrap();
        for expected in SYMBOLS {
            if &name[..end] != expected.as_bytes() {
                continue;
            }
            assert_eq!(u32::from(bytes[entry + 5]), text_section);
            let value = double_word(&bytes, entry + 8);
            assert!(
                values.insert(expected, value).is_none(),
                "unique {expected}"
            );
        }
    }
    for expected in SYMBOLS {
        assert!(
            values.contains_key(expected),
            "missing linked coordinate {expected}"
        );
    }

    // ld64's SymbolTableAtom::addGlobal emits N_ALT_ENTRY only for relocatable
    // object output. Final executable flags and LC_FUNCTION_STARTS may enumerate
    // public coordinates, so neither can prove input-atom ownership here.
    let slide = (std::ptr::addr_of!(TRAMPOLINE_END).addr() as u64)
        .checked_sub(values["_otter_native_trampoline_end"])
        .unwrap();
    let on_disk = |live: usize| (live as u64).checked_sub(slide).unwrap();
    let generic = on_disk(super::super::call_generic_entry as *const () as usize);
    let native = on_disk(super::super::call_native_entry as *const () as usize);
    let publisher = on_disk(super::super::call_request_entry as *const () as usize);
    let trampoline = on_disk(super::super::call_trampoline as *const () as usize);
    let live_coordinates = [
        std::ptr::addr_of!(GENERIC_END).addr(),
        std::ptr::addr_of!(NATIVE_END).addr(),
        std::ptr::addr_of!(PUBLISHER_END).addr(),
        std::ptr::addr_of!(HEADER_START).addr(),
        std::ptr::addr_of!(HEADER_END).addr(),
        std::ptr::addr_of!(CLASSIFIER).addr(),
        std::ptr::addr_of!(RESERVATION).addr(),
        std::ptr::addr_of!(INVOKE).addr(),
        std::ptr::addr_of!(TRAMPOLINE_END).addr(),
    ];
    for (name, live) in SYMBOLS.into_iter().zip(live_coordinates) {
        assert_eq!(
            values[name],
            on_disk(live),
            "exact loaded/file coordinate {name}"
        );
    }
    let body = |start: u64, end: u64| {
        assert!(start < end);
        let start = usize::try_from(start.checked_sub(text_address).unwrap()).unwrap();
        let end = usize::try_from(end.checked_sub(text_address).unwrap()).unwrap();
        text_bytes
            .get(start..end)
            .expect("complete linked body extent")
    };
    let generic_code = body(generic, values["_otter_native_generic_end"]);
    let native_code = body(native, values["_otter_native_selected_end"]);
    let publisher_code = body(publisher, values["_otter_native_publisher_end"]);
    let trampoline_code = body(trampoline, values["_otter_native_trampoline_end"]);
    assert!(publisher < values["_otter_native_header_start"]);
    assert!(values["_otter_native_header_start"] < values["_otter_native_header_end"]);
    assert!(values["_otter_native_header_end"] < values["_otter_native_publisher_end"]);
    assert!(trampoline < values["_otter_native_classifier"]);
    assert!(values["_otter_native_classifier"] < values["_otter_native_reservation"]);
    assert!(values["_otter_native_reservation"] < values["_otter_native_invoke"]);
    assert!(values["_otter_native_invoke"] < values["_otter_native_trampoline_end"]);

    #[cfg(target_arch = "aarch64")]
    {
        assert_eq!(word(&bytes, 4), 0x0100_000c, "actual ARM64 executable");
        arm64_branches_stay_with_their_body(generic_code, generic, &[publisher]);
        arm64_branches_stay_with_their_body(native_code, native, &[publisher]);
        arm64_branches_stay_with_their_body(publisher_code, publisher, &[trampoline]);
        arm64_branches_stay_with_their_body(trampoline_code, trampoline, &[]);
    }
    #[cfg(target_arch = "x86_64")]
    {
        assert_eq!(word(&bytes, 4), 0x0100_0007, "actual x86-64 executable");
        // The common native ABI tests execute these paths on x86. This test
        // proves the final file/mapping extents without inventing a partial
        // variable-length decoder or retaining another decoder dependency.
        let sizes = native_entry_code_sizes();
        assert_eq!(generic_code.len(), sizes.generic_selector_bytes as usize);
        assert_eq!(native_code.len(), sizes.native_selector_bytes as usize);
        assert_eq!(publisher_code.len(), sizes.request_publisher_bytes as usize);
        assert_eq!(trampoline_code.len(), sizes.trampoline_bytes as usize);
    }
}

#[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
fn arm64_branches_stay_with_their_body(code: &[u8], start: u64, tail_targets: &[u64]) {
    assert_eq!(code.len() % 4, 0);
    let end = start.checked_add(code.len() as u64).unwrap();
    let mut branches = 0;
    let mut tails = 0;
    for (index, bytes) in code.chunks_exact(4).enumerate() {
        let word = u32::from_le_bytes(bytes.try_into().unwrap());
        let branch = if word & 0xfc00_0000 == 0x1400_0000 {
            Some((word & 0x03ff_ffff, 26, true)) // B, excluding BL.
        } else if word & 0xff00_0010 == 0x5400_0000 || word & 0x7e00_0000 == 0x3400_0000 {
            Some(((word >> 5) & 0x7ffff, 19, false)) // B.cond / CBZ / CBNZ.
        } else if word & 0x7e00_0000 == 0x3600_0000 {
            Some(((word >> 5) & 0x3fff, 14, false)) // TBZ / TBNZ.
        } else {
            None
        };
        let Some((immediate, width, unconditional)) = branch else {
            continue;
        };
        branches += 1;
        let displacement = ((immediate as i32) << (32 - width)) >> (32 - width);
        let address = start + (index * 4) as u64;
        let destination = address
            .checked_add_signed(i64::from(displacement) * 4)
            .unwrap();
        assert_eq!(destination % 4, 0);
        if start <= destination && destination < end {
            continue;
        }
        assert!(
            unconditional && tail_targets.contains(&destination),
            "local branch {address:#x} -> {destination:#x} escaped [{start:#x},{end:#x})"
        );
        tails += 1;
    }
    assert!(
        branches > 0,
        "the actual body contains its branch instructions"
    );
    if !tail_targets.is_empty() {
        assert!(
            tails > 0,
            "actual tail jump reaches the sole downstream owner"
        );
    }
}
