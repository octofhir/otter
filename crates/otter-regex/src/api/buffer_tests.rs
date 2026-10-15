//! Exact live capture prefixes while native scratch crosses pattern widths.
//!
//! # Contents
//! - A wide native search handing its buffers to narrower and empty-group ones.
//! - Owned snapshots remaining independent of subsequent matcher writes.
//! - Progress marks excluded from lending, single and batch capture results.
//!
//! # Invariants
//! - The native tier's retained capacity never becomes a logical capture count.
//! - Results exclude progress marks even when matcher scratch retains them.
//!
//! # See also
//! - [`super`] owns lending searches and retained scratch.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use super::*;

#[test]
fn retained_native_capture_capacity_never_leaks_into_a_narrower_pattern() {
    let mut scratch = MatchScratch::default();
    let mut retained = Vec::new();
    for (pattern, text, groups) in [
        ("(a)(b)(c)(d)(e)", &b"abcde"[..], 5),
        ("(?<x>a)", &b"a"[..], 1),
        ("b", &b"b"[..], 0),
        ("(c)(d)", &b"cd"[..], 2),
    ] {
        let regex = Regex::compile_str(pattern, Flags::default()).unwrap();
        let code = native::compile::<u8>(regex.program()).expect("native-eligible capture fixture");
        code.prepare();
        let mut search = regex.find_latin1_with(text, 0, ExecConfig::default(), scratch);
        search.native = Some(&code);
        let batch = search.collect_batch(usize::MAX).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch.first().unwrap().captures.len(), groups);
        retained.push(batch);
        scratch = search.into_scratch();
    }
    assert_eq!(retained[0].first().unwrap().group(5), Some(4..5));
    assert_eq!(retained[1].first().unwrap().named_group("x"), Some(0..1));
    assert_eq!(retained[2].first().unwrap().captures.len(), 0);
    assert_eq!(retained[3].first().unwrap().group(2), Some(1..2));
}

#[test]
fn loop_progress_slots_never_become_captures_in_any_result_storage() {
    fn check<U: Unit>(regex: &Regex, text: &[U]) {
        let groups = regex.group_count() as usize;
        assert!(regex.program().slot_count() > regex.program().capture_slot_count());
        let code = native::compile::<U>(regex.program()).expect("native loop progress fixture");
        for tier in [None, Some(&code)] {
            let mut search = regex.find_on_tier(text, 0, ExecConfig::default(), tier);
            let view = search.next().unwrap().unwrap();
            assert_eq!(view.captures.len(), groups);
            assert_eq!(view.group(groups + 1), None);

            let mut search = regex.find_on_tier(text, 0, ExecConfig::default(), tier);
            let record = search.collect_first().unwrap().unwrap();
            assert_eq!(record.captures().len(), groups);
            assert_eq!(record.as_match().group(groups + 1), None);

            let mut search = regex.find_on_tier(text, 0, ExecConfig::default(), tier);
            let batch = search.collect_batch(usize::MAX).unwrap();
            assert!(!batch.is_empty());
            for view in batch.iter() {
                assert_eq!(view.captures.len(), groups);
                assert_eq!(view.group(groups + 1), None);
            }
        }
    }
    for pattern in ["(a?)*", "(?<x>a*)*"] {
        let regex = Regex::compile_str(pattern, Flags::default()).unwrap();
        check(&regex, b"aa b");
        check(&regex, &"aa b".encode_utf16().collect::<Vec<_>>());
    }
}
