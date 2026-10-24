//! Replacement templates expand into exact subject and template runs.
//!
//! # Contents
//! - Numeric, named, optional and duplicate-name substitutions.
//! - Prefix/suffix substitution from the original subject.
//! - Lone-surrogate preservation and Unicode empty matches.
//! - The spliced result's width and its unchanged subject on a miss.
//!
//! # Invariants
//! - Runs refer to the same immutable subject and template units.
//! - Tests compare exact code units, including unpaired surrogates.
//!
//! # See also
//! - `otter-difftest/corpus/regexp_replace_borrowed.js` covers collecting callbacks.

use super::*;

/// The units `pattern` replacing every match of `subject` with `template`
/// splices, assembled from the runs the replacement records.
fn render(
    pattern: &str,
    subject: CodeUnits<'_>,
    template: CodeUnits<'_>,
    unicode: bool,
) -> Vec<u16> {
    let regex = otter_regex::Regex::compile_str(
        pattern,
        otter_regex::Flags {
            unicode,
            ..Default::default()
        },
    )
    .unwrap();
    let config = otter_regex::ExecConfig::default();
    let named = regex.has_named_groups();
    let mut runs: SmallVec<[SpliceRun; 16]> = SmallVec::new();
    let mut next = 0;
    let mut visit = |matched: &crate::regexp::engine::Match<'_>| {
        if matched.range.start > next {
            runs.push(run(SUBJECT, next..matched.range.start));
        }
        expand_template(&mut runs, template, subject.len(), matched, named);
        next = matched.range.end;
    };
    match subject {
        CodeUnits::Latin1(bytes) => {
            let mut matches = regex.find_latin1(bytes, 0, config);
            while let Some(found) = matches.next() {
                visit(&found.unwrap());
            }
        }
        CodeUnits::Utf16(units) => {
            let mut matches = regex.find_utf16(units, 0, config);
            while let Some(found) = matches.next() {
                visit(&found.unwrap());
            }
        }
    }
    if next < subject.len() {
        runs.push(run(SUBJECT, next..subject.len()));
    }
    let mut out = Vec::new();
    for run in runs {
        let source = if run.source == SUBJECT {
            subject
        } else {
            template
        };
        let range = run.start as usize..(run.start + run.len) as usize;
        out.extend((range).map(|index| source.get(index).unwrap()));
    }
    out
}

fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

#[test]
fn templates_keep_optional_numeric_and_named_substitutions() {
    let out = render(
        "(?<head>a)(b)?",
        CodeUnits::Latin1(b"ab x a"),
        CodeUnits::Latin1(b"<$&:$1:$2:$12:$01:$0:$$:$<head>>"),
        false,
    );
    assert_eq!(out, units("<ab:a:b:a2:a:$0:$:a> x <a:a::a2:a:$0:$:a>"));
}

#[test]
fn templates_read_prefix_suffix_and_duplicate_names() {
    let out = render(
        "ab",
        CodeUnits::Latin1(b"abXab"),
        CodeUnits::Latin1(b"[$`][$']"),
        false,
    );
    assert_eq!(out, units("[][Xab]X[abX][]"));
    let out = render(
        "(?<x>a)|(?<x>b)",
        CodeUnits::Latin1(b"a-b"),
        CodeUnits::Latin1(b"<$<x>>"),
        false,
    );
    assert_eq!(out, units("<a>-<b>"));
}

#[test]
fn templates_keep_unmatched_dollars_and_unclosed_names_literal() {
    let out = render(
        "(?<n>a)",
        CodeUnits::Latin1(b"a"),
        CodeUnits::Latin1(b"$x$9$<n>$"),
        false,
    );
    assert_eq!(out, units("$x$9a$"));
    // A group name runs to the next `>`: none names nothing, and an
    // unknown name substitutes the empty string.
    let out = render(
        "(?<n>a)",
        CodeUnits::Latin1(b"a"),
        CodeUnits::Latin1(b"[$<n$<nope>]$<n"),
        false,
    );
    assert_eq!(out, units("[]$<n"));
}

#[test]
fn templates_keep_lone_surrogates() {
    let subject = [0x41, 0xd800, 0x42, 0xdfff, 0x43];
    let template = [0x4e2d, b'$' as u16, b'1' as u16];
    let out = render(
        "(.)",
        CodeUnits::Utf16(&subject),
        CodeUnits::Utf16(&template),
        false,
    );
    assert_eq!(
        out,
        [
            0x4e2d, 0x41, 0x4e2d, 0xd800, 0x4e2d, 0x42, 0x4e2d, 0xdfff, 0x4e2d, 0x43
        ]
    );
}

#[test]
fn unicode_empty_matches_advance_over_a_surrogate_pair() {
    let out = render(
        "(?:)",
        CodeUnits::Utf16(&[0xd83d, 0xde00]),
        CodeUnits::Latin1(b"|"),
        true,
    );
    assert_eq!(out, [b'|' as u16, 0xd83d, 0xde00, b'|' as u16]);
}

#[test]
fn splices_narrow_wide_runs_and_keep_wide_ones() {
    let mut heap = otter_gc::GcHeap::new().expect("heap");
    let wide = crate::string::JsString::from_utf16_units(&[0x41, 0x4e2d, 0x42], &mut heap).unwrap();
    let narrow = crate::string::JsString::from_str("xy", &mut heap).unwrap();
    let mut sources = [wide.handle(), narrow.handle()];
    let both = |start, len, source| SpliceRun { source, start, len };
    let spliced = crate::string::gc_body::splice_string_body(
        &mut heap,
        &mut sources,
        &[both(0, 1, 0), both(0, 2, 1), both(2, 1, 0)],
        &mut |_| {},
    )
    .unwrap();
    let spliced = crate::string::JsString::from_handle(spliced, &heap);
    assert!(
        spliced
            .with_latin1(&heap, |bytes| bytes == b"AxyB")
            .unwrap()
    );
    let spliced = crate::string::gc_body::splice_string_body(
        &mut heap,
        &mut sources,
        &[both(1, 2, 0), both(1, 1, 1)],
        &mut |_| {},
    )
    .unwrap();
    let spliced = crate::string::JsString::from_handle(spliced, &heap);
    assert_eq!(spliced.to_utf16_vec(&heap), [0x4e2d, 0x42, b'y' as u16]);
}
