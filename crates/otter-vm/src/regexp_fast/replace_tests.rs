//! Borrowed replacement templates preserve capture selection and code units.
//!
//! # Contents
//! - Numeric, named, optional and duplicate-name substitutions.
//! - Prefix/suffix substitution from the original subject.
//! - Lone-surrogate preservation, width narrowing and Unicode empty matches.
//!
//! # Invariants
//! - Matcher ranges refer to the same immutable borrowed subject.
//! - Tests compare exact code units, including unpaired surrogates.
//!
//! # See also
//! - `otter-difftest/corpus/regexp_replace_borrowed.js` covers collecting callbacks.

use super::*;

fn render(
    pattern: &str,
    subject: CodeUnits<'_>,
    template: CodeUnits<'_>,
    unicode: bool,
) -> UnitSink {
    let regex = otter_regex::Regex::compile_str(
        pattern,
        otter_regex::Flags {
            unicode,
            ..Default::default()
        },
    )
    .unwrap();
    let config = otter_regex::ExecConfig::default();
    let matches = match subject {
        CodeUnits::Latin1(bytes) => regex.find_latin1(bytes, 0, config).collect_batch(usize::MAX),
        CodeUnits::Utf16(units) => regex.find_utf16(units, 0, config).collect_batch(usize::MAX),
    }
    .unwrap();
    let named = matches
        .first()
        .is_some_and(|m| m.named_groups().next().is_some());
    build_template(subject, template, &matches, named).unwrap()
}

fn units(sink: UnitSink) -> Vec<u16> {
    match sink {
        UnitSink::Latin1(bytes) => bytes.into_iter().map(u16::from).collect(),
        UnitSink::Utf16(units) => units,
    }
}

#[test]
fn borrowed_templates_keep_optional_numeric_and_named_substitutions() {
    let out = render(
        "(?<head>a)(b)?",
        CodeUnits::Latin1(b"ab x a"),
        CodeUnits::Latin1(b"<$&:$1:$2:$12:$01:$0:$$:$<head>>"),
        false,
    );
    assert_eq!(
        units(out),
        "<ab:a:b:a2:a:$0:$:a> x <a:a::a2:a:$0:$:a>"
            .encode_utf16()
            .collect::<Vec<_>>()
    );
}

#[test]
fn borrowed_templates_read_prefix_suffix_and_duplicate_names() {
    let out = render(
        "ab",
        CodeUnits::Latin1(b"abXab"),
        CodeUnits::Latin1(b"[$`][$']"),
        false,
    );
    assert_eq!(
        units(out),
        "[][Xab]X[abX][]".encode_utf16().collect::<Vec<_>>()
    );
    let out = render(
        "(?<x>a)|(?<x>b)",
        CodeUnits::Latin1(b"a-b"),
        CodeUnits::Latin1(b"<$<x>>"),
        false,
    );
    assert_eq!(units(out), "<a>-<b>".encode_utf16().collect::<Vec<_>>());
}

#[test]
fn borrowed_templates_keep_lone_surrogates_when_widening() {
    let subject = [0x41, 0xd800, 0x42, 0xdfff, 0x43];
    let template = [0x4e2d, b'$' as u16, b'1' as u16];
    let out = render(
        "(.)",
        CodeUnits::Utf16(&subject),
        CodeUnits::Utf16(&template),
        false,
    );
    assert_eq!(
        units(out),
        [
            0x4e2d, 0x41, 0x4e2d, 0xd800, 0x4e2d, 0x42, 0x4e2d, 0xdfff, 0x4e2d, 0x43
        ]
    );
    assert_eq!(subject, [0x41, 0xd800, 0x42, 0xdfff, 0x43]);
}

#[test]
fn borrowed_two_byte_inputs_can_produce_a_one_byte_result() {
    let out = render(
        ".+",
        CodeUnits::Utf16(&[0x41, 0x42]),
        CodeUnits::Utf16(&[0x58]),
        false,
    );
    assert!(matches!(out, UnitSink::Latin1(bytes) if bytes == b"X"));
}

#[test]
fn borrowed_unicode_empty_matches_advance_over_a_surrogate_pair() {
    let out = render(
        "(?:)",
        CodeUnits::Utf16(&[0xd83d, 0xde00]),
        CodeUnits::Latin1(b"|"),
        true,
    );
    assert_eq!(units(out), [b'|' as u16, 0xd83d, 0xde00, b'|' as u16]);
}
