//! End-to-end matching tests for the Phase-1 engine: pattern string in, match
//! semantics out, exercised through the public `Regex` API exactly as a host VM
//! would drive it. Each test names the §22.2 feature it covers.

use otter_regex::{ExecConfig, Flags, Regex};

/// Compile `pattern` under `flag_str` and return the first match's overall
/// `(start, end)` plus a `Vec` of capture `(start, end)` (or `None`), searching
/// from offset 0.
fn first_match(
    pattern: &str,
    flag_str: &str,
    subject: &str,
) -> Option<(usize, usize, Vec<Option<(usize, usize)>>)> {
    let flags = Flags::from_str_lossy(flag_str);
    let re = Regex::compile_str(pattern, flags).expect("compile");
    let units: Vec<u16> = subject.encode_utf16().collect();
    let mut it = re.find_utf16(&units, 0, ExecConfig::default());
    match it.next() {
        Some(Ok(m)) => {
            let caps = m
                .captures
                .iter()
                .map(|c| c.as_ref().map(|r| (r.start, r.end)))
                .collect();
            Some((m.range.start, m.range.end, caps))
        }
        _ => None,
    }
}

fn matched_text(pattern: &str, flag_str: &str, subject: &str) -> Option<String> {
    let flags = Flags::from_str_lossy(flag_str);
    let re = Regex::compile_str(pattern, flags).expect("compile");
    let units: Vec<u16> = subject.encode_utf16().collect();
    let mut it = re.find_utf16(&units, 0, ExecConfig::default());
    match it.next() {
        Some(Ok(m)) => Some(String::from_utf16_lossy(&units[m.range.clone()])),
        _ => None,
    }
}

#[test]
fn literal_and_offset() {
    assert_eq!(
        first_match("bc", "", "abcd").map(|m| (m.0, m.1)),
        Some((1, 3))
    );
    assert_eq!(first_match("zz", "", "abcd"), None);
}

#[test]
fn dot_excludes_newline_unless_dotall() {
    assert_eq!(matched_text("a.b", "", "a\nb"), None);
    assert_eq!(matched_text("a.b", "s", "a\nb").as_deref(), Some("a\nb"));
}

#[test]
fn star_is_greedy_plus_lazy() {
    assert_eq!(matched_text("a+", "", "aaaa").as_deref(), Some("aaaa"));
    assert_eq!(matched_text("a+?", "", "aaaa").as_deref(), Some("a"));
    assert_eq!(matched_text("a*", "", "bbb").as_deref(), Some(""));
}

#[test]
fn counted_quantifier() {
    assert_eq!(matched_text("a{2,3}", "", "aaaa").as_deref(), Some("aaa"));
    assert_eq!(matched_text("a{2}", "", "aaaa").as_deref(), Some("aa"));
    assert_eq!(matched_text("a{2,}", "", "aaaa").as_deref(), Some("aaaa"));
}

#[test]
fn alternation_priority() {
    assert_eq!(matched_text("ab|abc", "", "abc").as_deref(), Some("ab"));
    assert_eq!(matched_text("abc|ab", "", "abc").as_deref(), Some("abc"));
}

#[test]
fn character_classes() {
    assert_eq!(matched_text("[a-c]+", "", "abcd").as_deref(), Some("abc"));
    assert_eq!(matched_text("[^a-c]+", "", "abcd").as_deref(), Some("d"));
    assert_eq!(matched_text("\\d+", "", "x123y").as_deref(), Some("123"));
    assert_eq!(matched_text("\\w+", "", " a_9 ").as_deref(), Some("a_9"));
    assert_eq!(
        matched_text("[\\d.]+", "", "v1.2.3!").as_deref(),
        Some("1.2.3")
    );
}

#[test]
fn anchors_and_boundaries() {
    assert_eq!(
        first_match("^abc", "", "abc").map(|m| (m.0, m.1)),
        Some((0, 3))
    );
    assert_eq!(first_match("^abc", "", "xabc"), None);
    assert_eq!(
        first_match("c$", "", "abc").map(|m| (m.0, m.1)),
        Some((2, 3))
    );
    assert_eq!(
        matched_text("\\bword\\b", "", "a word here").as_deref(),
        Some("word")
    );
    assert_eq!(first_match("\\bword\\b", "", "swordfish"), None);
}

#[test]
fn multiline_anchors() {
    assert_eq!(
        first_match("^b", "m", "a\nb\nc").map(|m| (m.0, m.1)),
        Some((2, 3))
    );
    assert_eq!(first_match("^b", "", "a\nb\nc"), None);
}

#[test]
fn capturing_groups() {
    let m = first_match("(a)(b)(c)", "", "abc").unwrap();
    assert_eq!((m.0, m.1), (0, 3));
    assert_eq!(m.2, vec![Some((0, 1)), Some((1, 2)), Some((2, 3))]);
}

#[test]
fn optional_group_is_unset() {
    let m = first_match("(a)(b)?c", "", "ac").unwrap();
    assert_eq!(m.2, vec![Some((0, 1)), None]);
}

#[test]
fn backreference() {
    assert_eq!(matched_text("(ab)\\1", "", "abab").as_deref(), Some("abab"));
    assert_eq!(matched_text("(ab)\\1", "", "abcd"), None);
}

#[test]
fn named_group_and_backref() {
    let flags = Flags::default();
    let re = Regex::compile_str("(?<pair>ab)\\k<pair>", flags).unwrap();
    let units: Vec<u16> = "abab".encode_utf16().collect();
    let m = re
        .find_utf16(&units, 0, ExecConfig::default())
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(m.named_group("pair"), Some(0..2));
}

#[test]
fn quantified_duplicate_named_group_clears_previous_iteration_capture() {
    let flags = Flags::default();
    let re = Regex::compile_str("(?:(?:(?<x>a)|(?<x>b)|c)\\k<x>){2}", flags).unwrap();
    let units: Vec<u16> = "aac".encode_utf16().collect();
    let m = re
        .find_utf16(&units, 0, ExecConfig::default())
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(m.range, 0..3);
    assert_eq!(m.named_group("x"), None);
}

#[test]
fn duplicate_named_backreference_uses_participating_capture() {
    let flags = Flags::default();
    let re = Regex::compile_str("(?:(?<x>a)|(?<x>b))\\k<x>", flags).unwrap();

    let aa: Vec<u16> = "aa".encode_utf16().collect();
    let aa_match = re
        .find_utf16(&aa, 0, ExecConfig::default())
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(aa_match.captures, vec![Some(0..1), None]);
    assert_eq!(aa_match.named_group("x"), Some(0..1));

    let bb: Vec<u16> = "bb".encode_utf16().collect();
    let bb_match = re
        .find_utf16(&bb, 0, ExecConfig::default())
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(bb_match.captures, vec![None, Some(0..1)]);
    assert_eq!(bb_match.named_group("x"), Some(0..1));
}

#[test]
fn duplicate_named_backreference_is_empty_when_no_candidate_participates() {
    let flags = Flags::default();
    let re = Regex::compile_str("^(?:(?<a>x)|(?<a>y)|z)\\k<a>$", flags).unwrap();
    let units: Vec<u16> = "z".encode_utf16().collect();
    let m = re
        .find_utf16(&units, 0, ExecConfig::default())
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(m.range, 0..1);
    assert_eq!(m.captures, vec![None, None]);
    assert_eq!(m.named_group("a"), None);
}

#[test]
fn case_insensitive_ascii() {
    assert_eq!(matched_text("abc", "i", "ABC").as_deref(), Some("ABC"));
    assert_eq!(
        matched_text("[a-z]+", "i", "HeLLo").as_deref(),
        Some("HeLLo")
    );
}

#[test]
fn lookahead() {
    // foo not followed by bar
    assert_eq!(
        matched_text("foo(?!bar)", "", "foobaz").as_deref(),
        Some("foo")
    );
    assert_eq!(first_match("foo(?!bar)", "", "foobar"), None);
    // positive lookahead
    assert_eq!(
        matched_text("foo(?=bar)", "", "foobar").as_deref(),
        Some("foo")
    );
}

#[test]
fn lookbehind() {
    // match digits preceded by '$'
    assert_eq!(
        matched_text("(?<=\\$)\\d+", "", "$42").as_deref(),
        Some("42")
    );
    assert_eq!(first_match("(?<=\\$)\\d+", "", "#42"), None);
    // negative lookbehind
    assert_eq!(
        matched_text("(?<!\\$)\\d+", "", "#42").as_deref(),
        Some("42")
    );
    let lookbehind_capture = first_match("(?<=(\\w){3})f", "", "abcdef").unwrap();
    assert_eq!((lookbehind_capture.0, lookbehind_capture.1), (5, 6));
    assert_eq!(lookbehind_capture.2[0], Some((2, 3)));
}

#[test]
fn unicode_surrogate_pair() {
    // U+1F600 is a surrogate pair in UTF-16; in u-mode `.` consumes both units.
    assert_eq!(
        matched_text(".", "u", "😀").map(|s| s.chars().count()),
        Some(1)
    );
    let m = first_match(".", "u", "😀").unwrap();
    assert_eq!((m.0, m.1), (0, 2));
}

#[test]
fn unicode_codepoint_escape() {
    assert_eq!(
        matched_text("\\u{1F600}", "u", "😀x").map(|s| s.chars().count()),
        Some(1)
    );
}

#[test]
fn global_iteration_yields_all_matches() {
    let flags = Flags::default();
    let re = Regex::compile_str("\\d+", flags).unwrap();
    let units: Vec<u16> = "a1b22c333".encode_utf16().collect();
    let found: Vec<String> = re
        .find_utf16(&units, 0, ExecConfig::default())
        .map(|m| String::from_utf16_lossy(&units[m.unwrap().range]))
        .collect();
    assert_eq!(found, vec!["1", "22", "333"]);
}

#[test]
fn empty_match_terminates() {
    // `a*` matches empty everywhere; iteration must terminate, not loop.
    let flags = Flags::default();
    let re = Regex::compile_str("a*", flags).unwrap();
    let units: Vec<u16> = "bb".encode_utf16().collect();
    let count = re.find_utf16(&units, 0, ExecConfig::default()).count();
    assert_eq!(count, 3); // positions 0, 1, 2
}

#[test]
fn property_escapes() {
    // General_Category group, value, Script, and negation.
    assert_eq!(
        matched_text("\\p{L}+", "u", "abc123").as_deref(),
        Some("abc")
    );
    assert_eq!(matched_text("\\p{Lu}+", "u", "ABcd").as_deref(), Some("AB"));
    assert_eq!(
        matched_text("\\p{Script=Greek}+", "u", "αβγx").map(|s| s.chars().count()),
        Some(3)
    );
    assert_eq!(
        matched_text("[\\p{L}\\d]+", "u", "ab12!").as_deref(),
        Some("ab12")
    );
    assert_eq!(matched_text("\\P{L}", "u", "5").as_deref(), Some("5"));
    // Binary property.
    assert!(matched_text("\\p{White_Space}", "u", "a b").is_some());
}

#[test]
fn unicode_simple_case_folding() {
    // U+212A KELVIN SIGN folds to 'k' under Simple Case Folding (i + u).
    assert!(matched_text("\\u{212a}", "iu", "k").is_some());
    assert!(matched_text("\\u{212a}", "iu", "K").is_some());
    // Non-unicode `i` must not map a non-ASCII code point onto an ASCII one.
    assert!(matched_text("\\u212a", "i", "k").is_none());
    assert!(matched_text("\\u212a", "i", "K").is_none());
    // Greek final/medial sigma fold together under i + u.
    assert!(matched_text("\\u03c3", "iu", "\u{03c2}").is_some());
}

#[test]
fn legacy_octal_escapes() {
    // \101 = octal 101 = 65 = 'A'.
    assert_eq!(matched_text("\\101", "", "ZAZ").as_deref(), Some("A"));
    // \8 is a NonOctalDecimalEscape: the literal digit.
    assert_eq!(matched_text("\\8", "", "x8y").as_deref(), Some("8"));
    // Octal inside a class.
    assert_eq!(matched_text("[\\101]", "", "ZAZ").as_deref(), Some("A"));
}

#[test]
fn sloppy_identity_escapes_of_letters() {
    // In non-unicode mode `\p`/`\P`/`\C` are identity escapes, not property
    // escapes or special forms.
    assert_eq!(matched_text("\\P", "", "P").as_deref(), Some("P"));
    assert_eq!(matched_text("O\\PQ", "", "OPQ").as_deref(), Some("OPQ"));
    assert_eq!(matched_text("\\C", "", "C").as_deref(), Some("C"));
}

#[test]
fn quantified_zero_width_assertion_terminates() {
    let s = "a bZ cZZ dZZZ eZZZZ";
    assert_eq!(matched_text(".(?=Z)*", "", s).as_deref(), Some("a"));
    assert_eq!(matched_text(".(?=Z)+", "", s).as_deref(), Some("b"));
    assert_eq!(matched_text(".(?=Z)?", "", s).as_deref(), Some("a"));
}

#[test]
fn empty_body_star_terminates() {
    // The empty-iteration guard (§22.2.2.5.1) keeps these from looping forever.
    assert_eq!(matched_text("(a*)*", "", "aaa").as_deref(), Some("aaa"));
    assert_eq!(matched_text("(a*)*", "", "").as_deref(), Some(""));
    assert_eq!(matched_text("(?:)*", "", "x").as_deref(), Some(""));
}

/// Auto-possessification (§4 of `REGEX_RESEARCH.md`): a greedy repeat whose
/// give-back is provably futile (disjoint required follower) must keep identical
/// match semantics, and an *overlapping* follower must NOT be possessified
/// (give-back is then genuinely needed).
#[test]
fn possessive_disjoint_preserves_matches() {
    // `[a-z.]+` is disjoint from the required `@`: possessifiable. Leftmost scan
    // must still find the email after skipping the futile leading runs.
    assert_eq!(
        matched_text("([a-z.]+)@([a-z.]+)", "i", "aaa bbb@ccc end").as_deref(),
        Some("bbb@ccc")
    );
    // Captures are unchanged by possessification.
    assert_eq!(
        first_match("([a-z.]+)@([a-z.]+)", "i", "a.b@example.com"),
        Some((0, 15, vec![Some((0, 3)), Some((4, 15))]))
    );
    // A run with no following `@` yields no match (the whole-run skip must not
    // wrongly report one).
    assert_eq!(matched_text("[a-z]+@", "", "abc def ghi"), None);
}

#[test]
fn overlapping_follower_is_not_possessified() {
    // `\w+` overlaps the required `\d` (a digit is a word char), so give-back to
    // the final digit is the only way to satisfy `\d`. Possessifying here would
    // wrongly drop the match — it must still succeed.
    assert_eq!(matched_text(r"\w+\d", "", "abc1").as_deref(), Some("abc1"));
    assert_eq!(matched_text(r"\w+\w", "", "ab").as_deref(), Some("ab"));
    // Disjoint variant of the same shape stays correct too.
    assert_eq!(
        matched_text(r"[a-z]+\d", "", "abc1").as_deref(),
        Some("abc1")
    );
    assert_eq!(matched_text(r"[a-z]+\d", "", "abcd"), None);
}

#[test]
fn possessive_lead_run_skip_is_leftmost() {
    // Several disjoint-follower runs precede the match; the per-run skip must
    // land on the leftmost real match, not overshoot it.
    assert_eq!(
        first_match("[a-z]+@[a-z]+", "", "one two three@four five").map(|m| (m.0, m.1)),
        Some((8, 18))
    );
}

#[test]
fn step_budget_aborts_catastrophic_backtracking() {
    // Classic ReDoS pattern; a tight budget must surface an error, not hang.
    let flags = Flags::default();
    let re = Regex::compile_str("(a+)+$", flags).unwrap();
    let units: Vec<u16> = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!".encode_utf16().collect();
    let cfg = ExecConfig {
        step_limit: Some(100_000),
    };
    let result = re.find_utf16(&units, 0, cfg).next();
    assert!(matches!(result, Some(Err(_))));
}
