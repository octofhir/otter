//! Differential tests: the native tier against the interpreter.
//!
//! Every case runs the whole leftmost search (every successive match) on both
//! tiers and requires the same matches, the same capture ranges and the same
//! number of steps — the step count pins the exploration order, not only its
//! result. Step budgets are swept so an exhausted budget must end both tiers
//! at the same point.

use super::{NativeCode, compile};
use crate::cursor::Unit;
use crate::{ExecConfig, ExecError, Flags, Regex};

type Found = (
    Vec<(
        core::ops::Range<usize>,
        Vec<Option<core::ops::Range<usize>>>,
    )>,
    Option<ExecError>,
    u64,
);

fn run<U: Unit>(
    regex: &Regex,
    text: &[U],
    start: usize,
    limit: u64,
    code: Option<&NativeCode>,
) -> Found {
    let mut matches = regex.find_on_tier(text, start, ExecConfig { step_limit: limit }, code);
    let mut found = Vec::new();
    let mut error = None;
    while let Some(item) = matches.next() {
        match item {
            Ok(m) => found.push((m.range.clone(), m.captures.iter().collect())),
            Err(e) => error = Some(e),
        }
    }
    (found, error, matches.steps_executed())
}

fn flags(spec: &str) -> Flags {
    Flags {
        ignore_case: spec.contains('i'),
        multiline: spec.contains('m'),
        dot_all: spec.contains('s'),
        ..Flags::default()
    }
}

/// Compare both tiers on `pattern` over every subject, from every start, at
/// both widths where the subject fits one byte per unit. Returns whether the
/// pattern compiled natively.
fn agree(pattern: &str, spec: &str, subjects: &[&str]) -> bool {
    agree_from(pattern, spec, subjects, true)
}

fn agree_from(pattern: &str, spec: &str, subjects: &[&str], every_start: bool) -> bool {
    let Ok(regex) = Regex::compile_str(pattern, flags(spec)) else {
        return false;
    };
    let wide = compile::<u16>(regex.program());
    let narrow = compile::<u8>(regex.program());
    let Some(wide) = wide else {
        assert!(narrow.is_none());
        return false;
    };
    let narrow = narrow.expect("both widths compile");
    for subject in subjects {
        let units: Vec<u16> = subject.encode_utf16().collect();
        let bytes: Option<Vec<u8>> = units.iter().map(|&u| u8::try_from(u).ok()).collect();
        let last = if every_start { units.len() + 1 } else { 0 };
        for start in 0..=last {
            let expected = run(&regex, &units, start, u64::MAX, None);
            let got = run(&regex, &units, start, u64::MAX, Some(&wide));
            assert_eq!(
                got, expected,
                "/{pattern}/{spec} on {subject:?} from {start} (wide)"
            );
            if let Some(bytes) = &bytes {
                let got = run(&regex, bytes, start, u64::MAX, Some(&narrow));
                assert_eq!(
                    got, expected,
                    "/{pattern}/{spec} on {subject:?} from {start} (narrow)"
                );
            }
            if start == 0 {
                for limit in budgets(expected.2) {
                    let expected = run(&regex, &units, 0, limit, None);
                    let got = run(&regex, &units, 0, limit, Some(&wide));
                    assert_eq!(
                        got, expected,
                        "/{pattern}/{spec} on {subject:?} limit {limit}"
                    );
                }
            }
        }
    }
    true
}

fn budgets(steps: u64) -> Vec<u64> {
    let mut limits = vec![0, 1, 2, 3, steps / 2, steps.saturating_sub(1), steps];
    limits.sort_unstable();
    limits.dedup();
    limits
}

const SUBJECTS: &[&str] = &[
    "",
    "a",
    "ab",
    "aab ba ab",
    "abcabcab",
    "xyz",
    "Aa bB cC",
    "foo bar  baz\nqux\r\nend",
    "aaaaaaaaaaaaaaaaaaaab",
    "http://example.com:8080/path?query=1#frag",
    "?a=1&bC=42&AB=x",
    "  leading and trailing  ",
    "_word-boundary_ 12 34",
    "\u{e9}t\u{e9} \u{c9}T\u{c9} \u{ff}\u{178}",
    "\u{100}a\u{2028}b\u{2029}c\u{4e00}",
    "abc\u{3b1}\u{391}\u{3c3}\u{3c2}",
];

#[test]
fn literals_classes_and_any() {
    for (pattern, spec) in [
        ("a", ""),
        ("ab", ""),
        ("abcabcab", ""),
        ("path?query", ""),
        ("[a-c]", ""),
        ("[^a-c ]", ""),
        ("\\w\\d", ""),
        ("\\s", ""),
        ("\\S+", ""),
        (".", ""),
        (".", "s"),
        ("a.c", ""),
        ("[\u{e9}\u{c9}]", ""),
        ("[\u{100}-\u{200}\u{4e00}]", ""),
        ("[^\u{100}-\u{200}]", ""),
        ("[\u{100}\u{102}\u{104}\u{106}\u{108}\u{10a}]", ""),
        ("\u{4e00}", ""),
        ("\u{3b1}\u{391}", ""),
    ] {
        assert!(agree(pattern, spec, SUBJECTS), "/{pattern}/{spec} compiles");
    }
}

#[test]
fn ignore_case() {
    for pattern in [
        "a",
        "AB",
        "[a-c]+",
        "\u{e9}t\u{e9}",
        "\u{ff}",
        "\u{178}",
        "\u{3c3}",
        "b\\w*",
        "(a)\\1",
    ] {
        assert!(agree(pattern, "i", SUBJECTS), "/{pattern}/i compiles");
    }
}

#[test]
fn repeats_and_alternation() {
    for (pattern, spec) in [
        ("a*", ""),
        ("a+b", ""),
        ("a*?b", ""),
        ("a+?", ""),
        ("[ab]{2,}", ""),
        ("\\s*$", ""),
        ("^\\s*|\\s*$", ""),
        ("a|ab|abc", ""),
        ("(?:ab|a)+c", ""),
        ("a{2,4}", ""),
        ("a{2,4}?", ""),
        (".*b", ""),
        (".*?b", "s"),
        ("x*", ""),
        ("(a|b)*c", ""),
        ("((a)|b)+", ""),
        ("(a?)*", ""),
        ("(a*)*b", ""),
        ("(?:a|)*b", ""),
        ("\\w+@?", ""),
        ("[\\w\\W]*(ab|bc)=", "i"),
        ("[\\w\\W]*(?:ab|B)", "i"),
        (".*?(?:b|c)a", ""),
        ("a*?[bc]$", ""),
        ("(\\w*)(\\W*)", ""),
    ] {
        assert!(agree(pattern, spec, SUBJECTS), "/{pattern}/{spec} compiles");
    }
}

#[test]
fn assertions_captures_and_backrefs() {
    for (pattern, spec) in [
        ("^a", ""),
        ("^\\w+", "m"),
        ("\\w+$", "m"),
        ("b$", ""),
        ("\\bab\\b", ""),
        ("\\Ba\\B", ""),
        ("\\b", ""),
        (
            "(((\\w+):\\/\\/)([^\\/:]*)(:(\\d+))?)?([^#?]*)(\\?([^#]*))?(#(.*))?",
            "",
        ),
        ("(a)(b)?(c)?", ""),
        ("(\\w)\\1", ""),
        ("(a)|\\1b", ""),
        ("(?<x>a)\\k<x>", ""),
        ("(a*)\\1", ""),
    ] {
        assert!(agree(pattern, spec, SUBJECTS), "/{pattern}/{spec} compiles");
    }
}

#[test]
fn uncovered_programs_stay_on_the_interpreter() {
    for (pattern, spec) in [("a(?=b)", ""), ("(?<!a)b", ""), ("a", "u"), ("\\p{L}", "u")] {
        let flags = Flags {
            unicode: spec.contains('u'),
            ..Flags::default()
        };
        let regex = Regex::compile_str(pattern, flags).expect("compiles");
        assert!(
            compile::<u16>(regex.program()).is_none(),
            "/{pattern}/{spec}"
        );
    }
}

/// A tiny fixed-seed generator.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % bound
    }
}

fn random_pattern(rng: &mut Lcg, depth: u32, groups: &mut u32) -> String {
    const ATOMS: &[&str] = &[
        "a",
        "b",
        "c",
        "A",
        ".",
        "[ab]",
        "[^a]",
        "\\w",
        "\\d",
        "\\s",
        "\\b",
        "^",
        "$",
        "ab",
        "\u{e9}",
        "[\u{100}-\u{4e00}]",
        "\\B",
    ];
    const QUANTS: &[&str] = &[
        "", "", "", "*", "+", "?", "*?", "+?", "??", "{1,2}", "{2,}", "{0,3}?",
    ];
    let mut out = String::new();
    for _ in 0..1 + rng.next(4) {
        let atom = if depth < 3 && rng.next(4) == 0 {
            let body = random_pattern(rng, depth + 1, groups);
            match rng.next(3) {
                0 => {
                    *groups += 1;
                    format!("({body})")
                }
                1 => format!("(?:{body})"),
                _ => format!("(?:{body}|{})", random_pattern(rng, depth + 1, groups)),
            }
        } else if *groups > 0 && rng.next(10) == 0 {
            format!("\\{}", 1 + rng.next(*groups as usize))
        } else {
            ATOMS[rng.next(ATOMS.len())].to_string()
        };
        out.push_str(&atom);
        out.push_str(QUANTS[rng.next(QUANTS.len())]);
    }
    if rng.next(5) == 0 {
        out.push('|');
        out.push_str(&random_pattern(rng, depth + 1, groups));
    }
    out
}

fn random_subject(rng: &mut Lcg, wide: bool) -> String {
    const NARROW: &[char] = &[
        'a', 'b', 'c', 'A', 'B', ' ', '\n', '1', '_', '\u{e9}', '\u{c9}',
    ];
    const WIDE: &[char] = &['a', 'b', '\u{100}', '\u{4e00}', '\u{2028}', '\u{3c3}'];
    (0..rng.next(14))
        .map(|_| {
            if wide && rng.next(3) == 0 {
                WIDE[rng.next(WIDE.len())]
            } else {
                NARROW[rng.next(NARROW.len())]
            }
        })
        .collect()
}

#[test]
fn random_programs_agree() {
    let mut rng = Lcg(0x5eed_2026_1013);
    let mut compiled = 0;
    for round in 0..600 {
        let mut groups = 0;
        let pattern = random_pattern(&mut rng, 0, &mut groups);
        let spec = ["", "i", "m", "s", "im"][rng.next(5)];
        let subjects: Vec<String> = (0..6)
            .map(|_| random_subject(&mut rng, round % 2 == 0))
            .collect();
        let subjects: Vec<&str> = subjects.iter().map(String::as_str).collect();
        if agree(&pattern, spec, &subjects) {
            compiled += 1;
        }
    }
    assert!(compiled > 400, "only {compiled} random programs compiled");
}

#[test]
fn deep_backtracking_grows_the_buffers() {
    // Every `a` leaves a frame and every iteration an undo entry, far past the
    // initial buffers.
    let subject: String = "a".repeat(5000);
    assert!(agree_from("(a|b)*c", "", &[&subject], false));
    assert!(agree_from("((a)|b)*$", "", &[&subject], false));
}
