//! Anchored searches: one candidate at the search position on both tiers.
//!
//! # Contents
//! - An anchored result equals the leftmost result exactly when that result
//!   begins at the search position, with identical work on both tiers.
//! - A failing candidate ends the search without trying later starts, so a
//!   catastrophic suffix costs nothing.
//! - Start anchors, multiline anchors, lookbehind, empty matches, subject
//!   ends and repeated anchored matches.
//! - Unicode searches starting between the halves of a surrogate pair begin
//!   at the pair's lead; other searches keep the exact unit.
//!
//! # Invariants
//! - Both tiers report the same matches, captures, errors and steps.
//!
//! # See also
//! - [`super::Matches::anchored`] selects the mode.

use super::*;
use core::ops::Range;

type Found = (
    Vec<(Range<usize>, Vec<Option<Range<usize>>>)>,
    Option<ExecError>,
    u64,
);

fn run<U: Unit>(
    regex: &Regex,
    text: &[U],
    start: usize,
    limit: u64,
    anchored: bool,
    tier: Option<&native::NativeCode>,
    max: usize,
) -> Found {
    let mut search = regex
        .find_on_tier(text, start, ExecConfig { step_limit: limit }, tier)
        .anchored(anchored);
    let mut found = Vec::new();
    let mut error = None;
    while found.len() < max {
        match search.next() {
            Some(Ok(m)) => found.push((m.range.clone(), m.captures.iter().collect())),
            Some(Err(e)) => {
                error = Some(e);
                break;
            }
            None => break,
        }
    }
    (found, error, search.steps_executed())
}

fn tiers<U: Unit>(regex: &Regex) -> Vec<Option<native::NativeCode>> {
    let mut tiers = vec![None];
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    if let Some(code) = native::compile::<U>(regex.program()) {
        tiers.push(Some(code));
    }
    tiers
}

fn flags(spec: &str) -> Flags {
    Flags {
        ignore_case: spec.contains('i'),
        multiline: spec.contains('m'),
        dot_all: spec.contains('s'),
        unicode: spec.contains('u'),
        ..Flags::default()
    }
}

/// Every start of every subject: the anchored first result is the leftmost
/// first result when that begins at the start, and nothing otherwise; both
/// tiers agree on everything an anchored search reports.
fn agree<U: Unit>(regex: &Regex, text: &[U]) -> bool {
    let tiers = tiers::<U>(regex);
    for start in 0..=text.len() + 1 {
        let mut reference = None;
        for tier in &tiers {
            let leftmost = run(regex, text, start, u64::MAX, false, tier.as_ref(), 1);
            let anchored = run(
                regex,
                text,
                start,
                u64::MAX,
                true,
                tier.as_ref(),
                usize::MAX,
            );
            let expected_start = super::search_start(text, start, regex.program.unicode);
            let first = leftmost
                .0
                .first()
                .filter(|(range, _)| range.start == expected_start);
            assert_eq!(anchored.0.first(), first, "start {start}");
            assert!(anchored.1.is_none());
            // Every later anchored match begins where the previous ended.
            for pair in anchored.0.windows(2) {
                let (previous, next) = (&pair[0].0, &pair[1].0);
                assert!(
                    next.start == previous.end
                        || (previous.is_empty() && next.start > previous.end)
                );
            }
            match &reference {
                None => reference = Some(anchored),
                Some(reference) => assert_eq!(reference, &anchored, "tiers differ at {start}"),
            }
        }
    }
    tiers.len() > 1
}

fn agree_both_widths(pattern: &str, spec: &str, subjects: &[&str]) -> bool {
    let regex = Regex::compile_str(pattern, flags(spec)).unwrap();
    let mut native = false;
    for subject in subjects {
        let units: Vec<u16> = subject.encode_utf16().collect();
        native |= agree(&regex, &units);
        if subject.chars().all(|c| (c as u32) < 0x100) {
            let bytes: Vec<u8> = subject.chars().map(|c| c as u8).collect();
            native |= agree(&regex, &bytes);
        }
    }
    native
}

#[test]
fn anchored_results_are_leftmost_results_at_the_start() {
    let mut native = 0;
    for (pattern, spec, subjects) in [
        ("b", "", &["abab", "", "b", "ba"][..]),
        ("a+", "", &["aab", "baa", "aaa"]),
        ("(a)|(b)", "", &["xaby", "ba"]),
        ("(?:)", "", &["", "ab"]),
        ("a*", "", &["baab", "aa"]),
        ("^a", "", &["aa", "ba"]),
        ("^a", "m", &["a\na", "b\nab"]),
        ("a$", "m", &["a\na", "ab\na"]),
        ("(?<=a)b", "", &["abab", "bb"]),
        ("(?<!a)b", "", &["abbb"]),
        ("\\bfoo", "", &["foo foo", "xfoo"]),
        ("[a-c]x", "i", &["AxbXcx", "xx"]),
        ("x(a+)+b", "", &["zxaaab", "xaab"]),
        ("abc|bcd", "", &["abcd", "bcd"]),
        ("(a?)*b", "", &["aab", "b"]),
        (".", "u", &["😀x", "x😀"]),
        ("(?:)", "u", &["😀x"]),
        ("\\u{1F600}", "u", &["😀x", "x😀"]),
        ("[\\s\\S]", "u", &["😀😀"]),
        ("^.", "u", &["😀x"]),
    ] {
        native += usize::from(agree_both_widths(pattern, spec, subjects));
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    assert!(native > 0, "no native-compiled fixture");
}

#[test]
fn a_failing_candidate_never_tries_a_later_start() {
    let regex = Regex::compile_str("x(a+)+b", Flags::default()).unwrap();
    let subject = format!("zx{}", "a".repeat(32));
    let bytes = subject.as_bytes();
    let units: Vec<u16> = subject.encode_utf16().collect();
    for tier in tiers::<u8>(&regex) {
        let (found, error, steps) = run(&regex, bytes, 0, 1_000, true, tier.as_ref(), 4);
        assert!(found.is_empty() && error.is_none() && steps <= 1, "{steps}");
    }
    for tier in tiers::<u16>(&regex) {
        let (found, error, steps) = run(&regex, &units, 0, 1_000, true, tier.as_ref(), 4);
        assert!(found.is_empty() && error.is_none() && steps <= 1, "{steps}");
    }
    // At the candidate itself the backtracking still runs, and still meets
    // the budget.
    let outcomes: Vec<_> = tiers::<u8>(&regex)
        .iter()
        .map(|tier| run(&regex, bytes, 1, 1_000, true, tier.as_ref(), 4))
        .collect();
    for outcome in &outcomes {
        assert!(outcome.0.is_empty());
        assert_eq!(outcome.1, Some(ExecError::StepLimitExceeded));
        assert_eq!(outcome, &outcomes[0]);
    }
}

#[test]
fn anchored_searches_stop_at_the_first_failure_and_the_subject_end() {
    let regex = Regex::compile_str("a", Flags::default()).unwrap();
    for tier in tiers::<u8>(&regex) {
        let (found, _, _) = run(&regex, b"aaba", 0, u64::MAX, true, tier.as_ref(), 8);
        assert_eq!(
            found.iter().map(|m| m.0.clone()).collect::<Vec<_>>(),
            [0..1, 1..2]
        );
        assert!(
            run(&regex, b"aaba", 4, u64::MAX, true, tier.as_ref(), 8)
                .0
                .is_empty()
        );
        assert!(
            run(&regex, b"aaba", 5, u64::MAX, true, tier.as_ref(), 8)
                .0
                .is_empty()
        );
    }
    let empty = Regex::compile_str("(?:)", Flags::default()).unwrap();
    for tier in tiers::<u8>(&empty) {
        let (found, _, _) = run(&empty, b"ab", 2, u64::MAX, true, tier.as_ref(), 8);
        assert_eq!(
            found.iter().map(|m| m.0.clone()).collect::<Vec<_>>(),
            std::slice::from_ref(&(2..2))
        );
        assert!(
            run(&empty, b"ab", 3, u64::MAX, true, tier.as_ref(), 8)
                .0
                .is_empty()
        );
    }
}

#[test]
fn unicode_searches_begin_at_the_lead_of_a_split_pair() {
    let units: Vec<u16> = "😀x".encode_utf16().collect();
    for (spec, expected) in [("u", Some(0..2)), ("", Some(1..2))] {
        let regex = Regex::compile_str(".", flags(spec)).unwrap();
        for anchored in [false, true] {
            for tier in tiers::<u16>(&regex) {
                let (found, _, _) = run(&regex, &units, 1, u64::MAX, anchored, tier.as_ref(), 1);
                assert_eq!(
                    found.first().map(|m| m.0.clone()),
                    expected,
                    "{spec} {anchored}"
                );
            }
        }
    }
    let lone = Regex::compile_str("\\udE00", flags("u")).unwrap();
    for tier in tiers::<u16>(&lone) {
        assert!(
            run(&lone, &units, 1, u64::MAX, true, tier.as_ref(), 1)
                .0
                .is_empty()
        );
    }
}
