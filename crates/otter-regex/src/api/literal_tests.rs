//! Complete scans agree with independent matcher execution and work limits.
//!
//! # Contents
//! - Both storage widths, every start, sticky searches and exhausted budgets.
//! - Declined Unicode, capture, assertion and truncated-needle programs.
//! - Scratch reuse across complete scans and general matcher searches.
//!
//! # Invariants
//! - Explicit tier searches bypass the complete scan and remain its oracle.
//! - Results include exact ranges, captures, errors and explored-step counts.

use super::*;

type Observation = (
    Vec<(
        core::ops::Range<usize>,
        Vec<Option<core::ops::Range<usize>>>,
    )>,
    Option<ExecError>,
    u64,
);

fn observe<U: Unit>(mut search: Matches<'_, '_, U>) -> Observation {
    let mut rows = Vec::new();
    let mut error = None;
    while let Some(found) = search.next() {
        match found {
            Ok(found) => rows.push((found.range, found.captures.iter().collect())),
            Err(found) => error = Some(found),
        }
    }
    (rows, error, search.steps_executed())
}

fn agree<U: Unit>(regex: &Regex, text: &[U]) {
    for start in 0..=text.len() + 1 {
        for sticky in [false, true] {
            for limit in [0, 1, 2, 3, u64::MAX] {
                let config = ExecConfig { step_limit: limit };
                let expected = observe(
                    regex
                        .find_on_tier(text, start, config, None)
                        .anchored(sticky),
                );
                let got = observe(
                    regex
                        .find_units(text, start, config, MatchScratch::default())
                        .anchored(sticky),
                );
                assert_eq!(got, expected, "start={start} sticky={sticky} limit={limit}");
            }
        }
    }
}

#[test]
fn literal_completion_preserves_ranges_and_candidate_work() {
    for pattern in ["=", "a", "aa", "aba", "NQ_VQ", "é", "éa", "Ā", "ĀΩ"] {
        let regex = Regex::compile_str(pattern, Flags::default()).unwrap();
        assert!(
            regex
                .program
                .required_literal
                .as_ref()
                .unwrap()
                .is_complete(),
            "{pattern}"
        );
        for text in [
            "",
            "=",
            "aaaa",
            "ababa",
            "aa ba a",
            "a=a=",
            "xNQ_VQ NQ_VQ",
            "éaé",
            "ĀΩĀ",
            "aĀΩx",
        ] {
            let units: Vec<u16> = text.encode_utf16().collect();
            agree(&regex, &units);
            if let Some(bytes) = units
                .iter()
                .map(|&u| u8::try_from(u).ok())
                .collect::<Option<Vec<_>>>()
            {
                agree(&regex, &bytes);
            }
        }
        // An odd-byte occurrence must not become a UTF-16 match. Also cover
        // exact lone surrogates without a lossy Rust-string round trip.
        agree(
            &regex,
            &[
                0x6100_u16, 0x0000, 0x0061, 0x0062, 0xD800, 0x0078, 0xD800, 0xDC00,
            ],
        );
    }
    let regex = Regex::compile_str("a", Flags::default()).unwrap();
    assert_eq!(
        observe(regex.find_utf16(&[0x6100, 0x0000, 0x0061], 0, ExecConfig::default())),
        (vec![(2..3, vec![])], None, 1),
    );
    // A byte needle for U+0100 sees 00 01 between every X and U+0101.
    // The unit scanner must ignore all those half-unit occurrences.
    let regex = Regex::compile_str("Ā", Flags::default()).unwrap();
    let mut text = [0x0058, 0x0101].repeat(256);
    text.push(0x0100);
    assert_eq!(
        observe(regex.find_utf16(&text, 0, ExecConfig::default())),
        (vec![(512..513, vec![])], None, 1),
    );
}

#[test]
fn complete_proof_never_accepts_partial_or_observable_programs() {
    for (pattern, flags) in [
        ("", Flags::default()),
        (
            "a",
            Flags {
                unicode: true,
                ..Flags::default()
            },
        ),
        (
            "a",
            Flags {
                ignore_case: true,
                ..Flags::default()
            },
        ),
        ("(a)", Flags::default()),
        ("(a){0}b", Flags::default()),
        ("(?<gone>a){0}b", Flags::default()),
        ("a$", Flags::default()),
        ("^a", Flags::default()),
        ("a|b", Flags::default()),
        ("a+", Flags::default()),
        ("a(?=b)", Flags::default()),
        ("\\uD800", Flags::default()),
        ("\\uD800x", Flags::default()),
    ] {
        let regex = Regex::compile_str(pattern, flags).unwrap();
        assert!(
            !regex
                .program
                .required_literal
                .as_ref()
                .is_some_and(RequiredLiteral::is_complete),
            "{pattern}"
        );
    }
    let pattern = "a".repeat(4096) + "b";
    let regex = Regex::compile_str(&pattern, Flags::default()).unwrap();
    assert!(
        !regex
            .program
            .required_literal
            .as_ref()
            .is_some_and(RequiredLiteral::is_complete)
    );
    let partial = "a".repeat(4096) + "c";
    assert!(
        regex
            .find_latin1(partial.as_bytes(), 0, ExecConfig::default())
            .next()
            .is_none()
    );
}

#[test]
fn elided_instructions_keep_declared_captures_and_names() {
    for pattern in ["(a){0}b", "b(a){0}", "(?<gone>a){0}b", "(a){0}(c){0}b"] {
        let regex = Regex::compile_str(pattern, Flags::default()).unwrap();
        for _ in 0..12 {
            let mut search = regex.find_latin1(b"xb", 0, ExecConfig::default());
            let matched = search.next().unwrap().unwrap();
            assert_eq!(matched.range, 1..2);
            assert_eq!(matched.captures.len(), regex.group_count() as usize);
            assert!(matched.captures.iter().all(|capture| capture.is_none()));
            if pattern.contains("gone") {
                assert_eq!(matched.named_groups().collect::<Vec<_>>(), [("gone", None)]);
            }
        }
        agree(&regex, b"xb");
        agree(&regex, &[0x0058_u16, 0x0062]);
    }
}

#[test]
fn retained_scratch_keeps_literal_and_matcher_results_separate() {
    let mut scratch = MatchScratch::default();
    let mut snapshots = Vec::new();
    for pattern in ["(a)(b)?", "a", "ab", "(?<first>a)(b)?", "b", "a+"] {
        let regex = Regex::compile_str(pattern, Flags::default()).unwrap();
        for _ in 0..12 {
            let mut search = regex.find_latin1_with(b"ab a b", 0, ExecConfig::default(), scratch);
            let batch = search.collect_batch(usize::MAX).unwrap();
            assert_eq!(
                batch.first().unwrap().captures.len(),
                regex.group_count() as usize
            );
            snapshots.push(batch);
            scratch = search.into_scratch();
        }
    }
    assert_eq!(snapshots[0].first().unwrap().group(2), Some(1..2));
    assert_eq!(snapshots[12].first().unwrap().range, 0..1);
    assert_eq!(snapshots[24].first().unwrap().range, 0..2);
    assert_eq!(
        snapshots[36].first().unwrap().named_group("first"),
        Some(0..1)
    );
}
