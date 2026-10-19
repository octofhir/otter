//! Capture selection through owned batches and borrowed match views.
//!
//! # Contents
//! - Group zero, optional groups and duplicate-name selection.
//! - Deterministic source order and distinct indices with equal ranges.
//! - Lazy name ownership, inline single matches and scratch-independent rows.
//!
//! # Invariants
//! - Fixture rows own all offsets and names before their views are read.
//!
//! # See also
//! - [`super`] owns capture storage and named selection.

use super::*;
use crate::{ExecConfig, Flags, Regex};

#[test]
fn a_batch_owns_names_once_and_views_never_clone_them() {
    let regex = Regex::compile_str("(?<x>a)(b)?", Flags::default()).unwrap();
    let names = &regex.program().names;
    let initial = Arc::strong_count(names);
    let mut search = regex.find_latin1(b"ab a ab a", 0, ExecConfig::default());
    {
        let view = search.next().unwrap().unwrap();
        assert_eq!(view.named_group("x"), Some(0..1));
        assert_eq!(Arc::strong_count(names), initial);
    }
    let batch = search.collect_batch(usize::MAX).unwrap();
    assert_eq!(batch.len(), 3);
    assert_eq!(Arc::strong_count(names), initial + 1);
    assert_eq!(
        batch.first().unwrap().captures.iter().collect::<Vec<_>>(),
        vec![Some(3..4), None]
    );
    for matched in batch.iter() {
        assert_eq!(matched.named_groups().count(), 1);
        assert_eq!(Arc::strong_count(names), initial + 1);
    }
    drop(batch);
    assert_eq!(Arc::strong_count(names), initial);
}

#[test]
fn misses_retain_no_names_and_captureless_records_allocate_no_slots() {
    let regex = Regex::compile_str("a", Flags::default()).unwrap();
    let names = &regex.program().names;
    let initial = Arc::strong_count(names);
    assert!(
        regex
            .find_latin1(b"bbb", 0, ExecConfig::default())
            .collect_first()
            .unwrap()
            .is_none()
    );
    let empty = regex
        .find_latin1(b"bbb", 0, ExecConfig::default())
        .collect_batch(usize::MAX)
        .unwrap();
    assert!(empty.is_empty());
    assert!(empty.names.is_none());
    assert!(!empty.slots.spilled());
    assert_eq!(Arc::strong_count(names), initial);
    let single = regex
        .find_latin1(b"a", 0, ExecConfig::default())
        .collect_first()
        .unwrap()
        .unwrap();
    assert!(single.captures.is_empty() && !single.captures.spilled());
    assert_eq!(single.as_match().group(0), Some(0..1));
}

#[test]
fn owned_rows_survive_scratch_reuse_and_pattern_destruction() {
    let batch = {
        let regex = Regex::compile_str("(?<x>a)|(b)", Flags::default()).unwrap();
        let mut first = regex.find_latin1(b"aba", 0, ExecConfig::default());
        let batch = first.collect_batch(usize::MAX).unwrap();
        let scratch = first.into_scratch();
        let mut next = regex.find_latin1_with(b"bbbb", 0, ExecConfig::default(), scratch);
        assert_eq!(next.collect_batch(usize::MAX).unwrap().len(), 4);
        batch
    };
    assert_eq!(batch.len(), 3);
    assert_eq!(
        batch.get(1).unwrap().captures.iter().collect::<Vec<_>>(),
        vec![None, Some(1..2)]
    );
    assert_eq!(batch.last().unwrap().named_group("x"), Some(2..3));
    let mut prefix = batch.clone();
    prefix.truncate(1);
    assert_eq!(prefix.len(), 1);
    assert_eq!(prefix.first().unwrap().range, 0..1);
    assert_eq!(batch.len(), 3);
}

#[test]
fn owned_record_survives_scratch_reuse_and_pattern_destruction() {
    let record = {
        let regex = Regex::compile_str("(?<x>a)|(b)", Flags::default()).unwrap();
        let mut search = regex.find_latin1(b"a", 0, ExecConfig::default());
        let record = search.collect_first().unwrap().unwrap();
        let scratch = search.into_scratch();
        let mut next = regex.find_latin1_with(b"bbb", 0, ExecConfig::default(), scratch);
        assert_eq!(next.collect_batch(2).unwrap().len(), 2);
        assert_eq!(next.collect_first().unwrap().unwrap().range, 2..3);
        record
    };
    assert_eq!(record.range, 0..1);
    assert_eq!(
        record.captures().iter().collect::<Vec<_>>(),
        vec![Some(0..1), None]
    );
    assert_eq!(record.as_match().named_group("x"), Some(0..1));
}

fn mk(range: Range<usize>, caps: Vec<Option<Range<usize>>>, names: &[&str]) -> MatchBatch {
    let names: Arc<[String]> = names.iter().map(|name| (*name).to_string()).collect();
    let mut slots = vec![Some(range.start), Some(range.end)];
    for capture in &caps {
        slots.push(capture.as_ref().map(|range| range.start));
        slots.push(capture.as_ref().map(|range| range.end));
    }
    let mut batch = MatchBatch::new(slots.len());
    batch.push(Slots::Interpreted(&slots), &names);
    batch
}

#[test]
fn group_zero_is_overall_match() {
    let matches = mk(2..5, vec![Some(2..3)], &["a"]);
    let m = matches.first().unwrap();
    assert_eq!(m.group(0), Some(2..5));
    assert_eq!(m.group(1), Some(2..3));
    assert_eq!(m.group(2), None);
}

#[test]
fn named_group_lookup() {
    let matches = mk(0..3, vec![Some(0..1), None], &["x", "y"]);
    let m = matches.first().unwrap();
    assert_eq!(m.named_group("x"), Some(0..1));
    assert_eq!(m.named_group("y"), None);
    assert_eq!(m.named_group("z"), None);
    assert_eq!(m.named_group(""), None);
}

/// ES2025 duplicate named groups: the same name appears in two
/// alternatives; the matched alternative's range wins.
#[test]
fn duplicate_named_group_prefers_match() {
    let matches = mk(0..1, vec![None, Some(0..1)], &["dup", "dup"]);
    let m = matches.first().unwrap();
    assert_eq!(m.named_group_index("dup"), Some(2));
    assert_eq!(m.named_group("dup"), Some(0..1));
}

#[test]
fn unmatched_named_group_retains_its_declared_index() {
    let matches = mk(0..0, vec![None, None, None], &["dup", "other", "dup"]);
    let m = matches.first().unwrap();
    assert_eq!(m.named_group_index("dup"), Some(1));
    assert_eq!(m.named_group_index("other"), Some(2));
    assert_eq!(m.named_group_index("missing"), None);
    assert_eq!(m.named_group_index(""), None);
    assert_eq!(m.named_group("dup"), None);
    assert_eq!(
        m.named_groups().collect::<Vec<_>>(),
        vec![("dup", None), ("other", None)]
    );
}

#[test]
fn equal_ranges_preserve_distinct_capture_indices() {
    let matches = mk(0..1, vec![Some(0..1), Some(0..1)], &["outer", "inner"]);
    let m = matches.first().unwrap();
    assert_eq!(m.named_group_index("outer"), Some(1));
    assert_eq!(m.named_group_index("inner"), Some(2));
    assert_eq!(m.named_group("outer"), m.named_group("inner"));
}

#[test]
fn named_groups_iter_is_deduped_and_ordered() {
    let matches = mk(0..1, vec![Some(0..1), None, None], &["dup", "dup", "other"]);
    let m = matches.first().unwrap();
    let collected: Vec<_> = m.named_groups().map(|(n, r)| (n.to_string(), r)).collect();
    assert_eq!(
        collected,
        vec![("dup".to_string(), Some(0..1)), ("other".to_string(), None)]
    );
}
