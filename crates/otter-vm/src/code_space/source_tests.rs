//! Immutable same-URL sources and code-versus-diagnostic lifetimes.
//!
//! # Contents
//! - Foreign function ownership selects its defining source after URL reuse.
//! - An eager line slice survives code eviction without retaining the chunk.
//!
//! # Invariants
//! - Code and source admission use the existing shared resource account.
//! - A diagnostic keeps the original full text lease until its last slice drops.
//! - No diagnostic function ID enters executable liveness or pins a payload.
//!
//! # See also
//! - `super::snapshot` owns accounted capture and independent table restore.

use super::*;
use otter_resource::SharedSource;
use std::collections::BTreeMap;

fn sources(account: &ResourceAccount, text: &str) -> SourceRegistry {
    SourceRegistry::new(
        BTreeMap::from([(
            "same.js".to_owned(),
            SharedSource::admit(account, text.to_owned()).unwrap(),
        )]),
        account,
    )
    .unwrap()
}

fn current(account: &ResourceAccount) -> u64 {
    account
        .snapshot()
        .get(ResourceClass::SourceModuleBytes)
        .current()
}

#[test]
fn same_url_foreign_owner_and_eager_line_do_not_pin_evicted_executable() {
    let account = ResourceAccount::default();
    let space = Arc::new(CodeSpace::default());
    let text_a = "first A\né𝄞 exact original\nlast A";
    let a_sources = sources(&account, text_a);
    // A position query builds A's line index, part of A's source charge.
    assert_eq!(a_sources.get("same.js").unwrap().line_col(0), (1, 1));
    let a_source_charge = current(&account);
    let first = space
        .link_evictable_module(
            crate::test_support::minimal_bytecode_module("same.js"),
            a_sources,
            &account,
        )
        .unwrap();
    let first_id = first.function_base();
    let second = space
        .link_module(
            crate::test_support::minimal_bytecode_module("same.js"),
            sources(&account, "different B\nsecond replacement\nextra B"),
            &account,
        )
        .unwrap();
    let foreign = second.for_function(first_id).unwrap();
    let original = foreign.source("same.js").unwrap();
    assert_eq!(original.line_text(2), Some("é𝄞 exact original"));
    assert_eq!(original.line_col(16), (2, 6));
    assert_eq!(
        second.source("same.js").unwrap().line_text(2),
        Some("second replacement")
    );
    let position = crate::ErrorSourcePosition {
        source_line: original.line_source(2).unwrap(),
        script_name: "same.js".to_owned(),
        line_number: 2,
        start_column: 5,
    };
    let line_pointer = position.source_line.as_ref().as_ptr();
    let cloned = position.clone();
    assert_eq!(cloned.source_line.as_ref().as_ptr(), line_pointer);
    drop(foreign);
    drop(first);
    let candidates = space.eviction_candidates();
    let [candidate] = candidates.as_slice() else {
        panic!("only A is evictable and no captured diagnostic retains its payload");
    };
    let candidate = *candidate;
    let before_eviction = current(&account);
    let after_eviction =
        before_eviction - candidate.retained_bytes - (a_source_charge - text_a.len() as u64);
    assert_eq!(
        space.evict_candidate(candidate),
        ChunkEvictionResult::Evicted {
            retained_bytes: candidate.retained_bytes
        }
    );
    assert!(matches!(
        space.resolve_chunk(first_id),
        ChunkResolution::Evicted { .. }
    ));
    assert_eq!(cloned.source_line.as_ref(), "é𝄞 exact original");
    assert_eq!(
        current(&account),
        after_eviction,
        "payload/source-index charges are gone; immutable directory/tombstones and one full original text charge remain"
    );
    drop(position);
    assert_eq!(current(&account), after_eviction);
    drop(cloned);
    assert_eq!(current(&account), after_eviction - text_a.len() as u64);
    drop(second);
    drop(space);
    assert_eq!(current(&account), 0);
}
