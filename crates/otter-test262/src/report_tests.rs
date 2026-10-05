//! Canonical report ownership, coverage, identity and diagnostic regressions.

use super::*;
use crate::results::SkipReason;

fn row(path: &str, outcome: Outcome) -> TestResult {
    TestResult {
        path: path.to_owned(),
        esid: Some("sec-test".to_owned()),
        features: vec!["original-feature".to_owned()],
        outcome,
        wall_ms: 123,
    }
}

fn report(rows: Vec<TestResult>) -> Baseline {
    Baseline::from_results(
        rows,
        crate::provenance::synthetic(),
        "corpus",
        "engine",
        "now",
    )
    .unwrap()
}

#[test]
fn canonical_rows_roundtrip_every_outcome_and_typed_skip_with_full_diagnostics() {
    let mut outcomes = vec![
        Outcome::Pass,
        Outcome::Fail {
            reason: "full failure".to_owned(),
            stack: Some("frame1\nframe2".to_owned()),
        },
        Outcome::Crash {
            panic: "panic detail".to_owned(),
        },
        Outcome::Timeout { ms: 30000 },
        Outcome::OutOfMemory { bytes: 123456 },
    ];
    outcomes.extend(
        [
            SkipReason::Feature {
                feature: "Atomics".to_owned(),
            },
            SkipReason::Flag {
                flag: "module".to_owned(),
            },
            SkipReason::Ignored {
                pattern: "skip/me".to_owned(),
            },
            SkipReason::KnownPanic {
                pattern: "known/panic".to_owned(),
            },
            SkipReason::SourceTooLarge {
                bytes: 2097153,
                limit: 2097152,
            },
            SkipReason::MissingFrontmatter,
            SkipReason::NoStrictnessVariant,
        ]
        .into_iter()
        .map(|reason| Outcome::Skipped { reason }),
    );
    let original = report(
        outcomes
            .into_iter()
            .enumerate()
            .map(|(index, outcome)| row(&format!("language/test/rows/{index:02}.js"), outcome))
            .collect(),
    );
    let json = original.to_json_pretty().unwrap();
    assert!(!json.contains("failing_tests"));
    let decoded: Baseline = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.tests, original.tests);
    assert_eq!(decoded.runner, original.runner);
    assert_eq!(decoded.totals.total, 12);
    assert_eq!(decoded.totals.skipped, 7);
    assert_eq!(decoded.totals.passed, 1);
}

#[test]
fn duplicate_pass_and_skip_paths_are_rejected_at_construction_and_decode() {
    let first = row("language/x/y/a.js", Outcome::Pass);
    let skip = row(
        &first.path,
        Outcome::Skipped {
            reason: SkipReason::MissingFrontmatter,
        },
    );
    assert!(
        Baseline::from_results(
            vec![first.clone(), skip.clone()],
            crate::provenance::synthetic(),
            "c",
            "e",
            "now"
        )
        .is_err()
    );
    let mut wire = serde_json::to_value(report(vec![first])).unwrap();
    wire["tests"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(skip).unwrap());
    assert!(serde_json::from_value::<Baseline>(wire).is_err());
}

#[test]
fn loader_rejects_inconsistent_rollups_and_aggregate_only_data() {
    let original = serde_json::to_value(report(vec![row(
        "built-ins/Array/from/a.js",
        Outcome::Pass,
    )]))
    .unwrap();
    for mutation in [
        "totals",
        "by_section",
        "missing_rows",
        "lossy_carrier",
        "unknown_skip",
    ] {
        let mut wire = original.clone();
        match mutation {
            "totals" => wire["totals"]["passed"] = 0.into(),
            "by_section" => wire["by_section"]["built-ins/Array/from"]["total"] = 2.into(),
            "missing_rows" => {
                wire.as_object_mut().unwrap().remove("tests");
            }
            "lossy_carrier" => wire["failing_tests"] = serde_json::json!([]),
            "unknown_skip" => {
                wire["tests"][0]["outcome"] =
                    serde_json::json!({"kind":"skipped","feature":"legacy"})
            }
            _ => unreachable!(),
        }
        assert!(
            serde_json::from_value::<Baseline>(wire).is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn exact_coverage_rejects_equal_count_path_swaps_and_duplicate_expectations() {
    let report = report(vec![
        row("language/x/y/a.js", Outcome::Pass),
        row(
            "language/x/y/c.js",
            Outcome::Skipped {
                reason: SkipReason::MissingFrontmatter,
            },
        ),
    ]);
    let expected = ["language/x/y/a.js", "language/x/y/b.js"];
    assert!(
        matches!(report.validate_coverage(expected), Err(ReportError::Coverage { missing, unexpected })
        if missing == ["language/x/y/b.js"] && unexpected == ["language/x/y/c.js"])
    );
    assert!(
        report
            .validate_coverage([expected[0], expected[0]])
            .is_err()
    );
    assert!(
        report
            .validate_coverage(report.tests.iter().map(|test| test.path.as_str()))
            .is_ok()
    );
}

#[test]
fn merge_checks_all_outcome_collisions_and_actual_runner_identity() {
    let a = report(vec![row("language/x/y/a.js", Outcome::Pass)]);
    let b = report(vec![row(
        "language/x/y/b.js",
        Outcome::Skipped {
            reason: SkipReason::MissingFrontmatter,
        },
    )]);
    let joined = Baseline::merge(
        vec![("a".to_owned(), a.clone()), ("b".to_owned(), b.clone())],
        "later",
    )
    .unwrap();
    assert_eq!(joined.totals.total, 2);
    assert_eq!(joined.tests[1], b.tests[0]);
    assert!(matches!(
        Baseline::merge(
            vec![("a".to_owned(), a.clone()), ("again".to_owned(), a.clone())],
            "now"
        ),
        Err(ReportError::MergeCollision { .. })
    ));
    for change in [
        "engine",
        "corpus",
        "digest",
        "target",
        "assertions",
        "skip_policy",
        "timeout",
    ] {
        let mut different = b.clone();
        match change {
            "engine" => different.engine_commit = "other".to_owned(),
            "corpus" => different.test262_commit = "other".to_owned(),
            "digest" => different.runner.executable_sha256 = "1".repeat(64),
            "target" => different.runner.target = "other".to_owned(),
            "assertions" => different.runner.debug_assertions = false,
            "skip_policy" => different
                .runner
                .semantic_config
                .skip_features
                .push("Atomics".to_owned()),
            "timeout" => different.runner.semantic_config.timeout_ms += 1,
            _ => unreachable!(),
        }
        assert!(
            Baseline::merge(
                vec![("a".to_owned(), a.clone()), ("b".to_owned(), different)],
                "now"
            )
            .is_err(),
            "{change}"
        );
    }
}

#[test]
fn normalized_paths_and_sorted_wire_are_required() {
    for path in [
        "",
        "/a.js",
        "../a.js",
        "a//b.js",
        "a\\b.js",
        "a/./b.js",
        "a_FIXTURE.js",
        "a.ts",
    ] {
        assert!(
            Baseline::from_results(
                vec![row(path, Outcome::Pass)],
                crate::provenance::synthetic(),
                "c",
                "e",
                "now"
            )
            .is_err(),
            "{path}"
        );
    }
    let mut wire = serde_json::to_value(report(vec![
        row("b.js", Outcome::Pass),
        row("a.js", Outcome::Pass),
    ]))
    .unwrap();
    wire["tests"].as_array_mut().unwrap().reverse();
    assert!(serde_json::from_value::<Baseline>(wire).is_err());
}

#[test]
fn nullable_diagnostics_are_required_and_skip_bound_must_be_real() {
    let baseline = report(vec![row(
        "language/x/y/a.js",
        Outcome::Fail {
            reason: "detail".to_owned(),
            stack: None,
        },
    )]);
    let original = serde_json::to_value(&baseline).unwrap();
    for field in ["esid", "stack"] {
        let mut wire = original.clone();
        if field == "esid" {
            wire["tests"][0].as_object_mut().unwrap().remove(field);
        } else {
            wire["tests"][0]["outcome"]
                .as_object_mut()
                .unwrap()
                .remove(field);
        }
        assert!(
            serde_json::from_value::<Baseline>(wire).is_err(),
            "missing {field}"
        );
    }
    let invalid = row(
        "language/x/y/a.js",
        Outcome::Skipped {
            reason: SkipReason::SourceTooLarge {
                bytes: 42,
                limit: 42,
            },
        },
    );
    assert!(
        Baseline::from_results(
            vec![invalid.clone()],
            crate::provenance::synthetic(),
            "c",
            "e",
            "now"
        )
        .is_err()
    );
    assert!(serde_json::from_value::<TestResult>(serde_json::to_value(invalid).unwrap()).is_err());
}
