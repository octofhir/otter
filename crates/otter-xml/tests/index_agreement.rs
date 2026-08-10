//! The vector classifier and the portable one must answer identically.
//!
//! The kernels are hand-written intrinsics, so this is the gate that keeps
//! them honest: same positions, same first failure, on random input, on every
//! alignment of a block boundary, and on documents larger than one block.

use otter_xml::index::bytes_with;
use otter_xml::simd::{BLOCK, Classifier, classify_scalar, kernels};

/// What one indexing run produced.
type Answer = (Result<(), String>, Vec<u32>);

fn index_with(document: &[u8], validate_utf8: bool, classify: Classifier) -> Answer {
    let mut out = Vec::new();
    let result =
        bytes_with(document, validate_utf8, &mut out, classify).map_err(|err| err.to_string());
    (result, out)
}

/// Every kernel this machine can run must give what the portable one gives.
fn assert_agree(document: &[u8], note: &str) {
    for validate_utf8 in [true, false] {
        let portable = index_with(document, validate_utf8, classify_scalar);
        for (name, classify) in kernels() {
            let answer = index_with(document, validate_utf8, classify);
            assert_eq!(
                answer,
                portable,
                "{name}: {note} (validate_utf8={validate_utf8}) for {:?}",
                String::from_utf8_lossy(document)
            );
        }
    }
}

#[test]
fn this_machine_runs_at_least_one_kernel_besides_the_portable_one() {
    let names: Vec<&str> = kernels().iter().map(|(name, _)| *name).collect();
    println!("kernels: {names:?}");
    assert_eq!(names.last(), Some(&"scalar"));
    if cfg!(any(target_arch = "aarch64", target_arch = "x86_64")) {
        assert!(names.len() > 1, "no vector kernel on {names:?}");
    }
}

/// A deterministic generator, so a failure is always reproducible.
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Bytes that make the classification interesting: the structural set, the
/// controls on both sides of the allowed ones, the nibble neighbours of the
/// structural bytes, and lead and continuation bytes both well and ill formed.
const ALPHABET: &[u8] = &[
    b'<', b'>', b'&', b'\r', b'\n', b'\t', b' ', b'=', b'"', b'\'', b'/', b'?', b'!', b'-', b'[',
    b']', b'a', b'6', b'.', b',', b';', 0x00, 0x01, 0x08, 0x0B, 0x0C, 0x0E, 0x19, 0x1A, 0x1D, 0x1F,
    0x7F, 0x80, 0xBF, 0xC0, 0xC2, 0xC3, 0xA9, 0xE2, 0x82, 0xAC, 0xED, 0xA0, 0xEF, 0xBF, 0xBE, 0xF0,
    0x9F, 0x99, 0x82, 0xF5, 0xFF,
];

#[test]
fn the_kernel_agrees_with_the_portable_classifier_on_random_input() {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    for round in 0..20_000 {
        let len = (xorshift(&mut state) % 300) as usize;
        let document: Vec<u8> = (0..len)
            .map(|_| ALPHABET[(xorshift(&mut state) as usize) % ALPHABET.len()])
            .collect();
        assert_agree(&document, &format!("round {round}"));
    }
}

#[test]
fn agreement_holds_at_every_offset_of_a_block_boundary() {
    // A run of padding puts the interesting bytes at each position in and
    // around a block, including a multi-byte sequence split across one.
    for pad in 0..(3 * BLOCK) {
        let mut document = b"x".repeat(pad);
        document.extend_from_slice("<a b='é'>漢\r\n&amp;\u{1}".as_bytes());
        assert_agree(&document, &format!("pad {pad}"));

        let mut document = b"y".repeat(pad);
        document.extend_from_slice(b"\xEF\xBF\xBE"); // U+FFFE, straddling
        assert_agree(&document, &format!("noncharacter at {pad}"));

        let mut document = b"z".repeat(pad);
        document.extend_from_slice(b"\xE2\x82"); // truncated at the end
        assert_agree(&document, &format!("truncated at {pad}"));
    }
}

#[test]
fn agreement_holds_on_a_document_of_many_blocks() {
    let mut document = String::from("<root>");
    let mut at = 0;
    while document.len() < 64 * BLOCK {
        document.push_str(&format!(
            "<item id=\"{at}\" name='n{at}'>\r\n\t<v k=\"a=b\">text {at} &amp; more > here é漢</v>\
             <w x=\"1\ty\"/></item>\n"
        ));
        at += 1;
    }
    document.push_str("</root>");
    assert_agree(document.as_bytes(), "generated document");
}

#[test]
fn agreement_holds_on_the_edges_of_length() {
    for len in 0..=(2 * BLOCK) {
        let document = b"<".repeat(len);
        assert_agree(&document, &format!("all structural, len {len}"));
        let document = b"x".repeat(len);
        assert_agree(&document, &format!("nothing structural, len {len}"));
        let mut document = b"x".repeat(len);
        document.push(0x01);
        assert_agree(&document, &format!("forbidden after {len}"));
    }
}

#[test]
fn every_byte_value_classifies_the_same_way_at_every_position() {
    for at in [0usize, 1, 15, 16, 31, 32, 63, 64, 65, 127] {
        for byte in 0..=0xFFu8 {
            let mut document = b"x".repeat(at + 1);
            document[at] = byte;
            assert_agree(&document, &format!("byte {byte:#04x} at {at}"));
        }
    }
}
