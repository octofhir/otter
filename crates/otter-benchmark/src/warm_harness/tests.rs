//! Source-only complete-work and strict protocol regression proofs.
//!
//! # Contents
//! - All seven full canonical fixtures, original span hashes and work ASTs.
//! - Changed driver/reserved-binding/reset writer rejection.
//! - Exact nanosecond records and partial/foreign/mismatched observation rejection.
//!
//! # Invariants
//! - These tests parse source and synthesize protocol data; no JS engine runs.
//! - Passing them is not cross-engine semantic or performance evidence.
//!
//! # See also
//! - The later capture driver owns real engine/host/version/oracle validation.

use super::*;
use oxc_span::ContentEq;
use std::collections::BTreeMap;

fn fixture(anchor: WarmAnchor) -> &'static str {
    match anchor {
        WarmAnchor::Fib => include_str!("../../../../benchmarks/fixtures/fixed-work/fib.js"),
        WarmAnchor::MegaMethod => {
            include_str!("../../../../benchmarks/fixtures/fixed-work/mega_method.js")
        }
        WarmAnchor::AstCtor => {
            include_str!("../../../../benchmarks/fixtures/fixed-work/ast_ctor.js")
        }
        WarmAnchor::Typescript => include_str!("../../../../benchmarks/fixtures/fixed-work/ts.js"),
        WarmAnchor::Crypto => include_str!("../../../../benchmarks/fixtures/fixed-work/crypto.js"),
        WarmAnchor::Zlib => include_str!("../../../../benchmarks/fixtures/fixed-work/zlib.js"),
        WarmAnchor::EarleyBoyer => {
            include_str!("../../../../benchmarks/fixtures/fixed-work/earley-boyer.js")
        }
    }
}
fn request(anchor: WarmAnchor) -> WarmHarnessRequest {
    WarmHarnessRequest {
        source: WarmSource {
            anchor,
            source_name: format!("{}.js", anchor.name()),
            original: fixture(anchor).into(),
            expected_sha256: anchor.original_sha256().into(),
        },
        sampling: SamplingPlan {
            warmup_count: 3,
            sample_count: 5,
            iterations_per_sample: None,
            timeout_ms: None,
        },
    }
}

#[test]
fn all_seven_complete_sources_and_work_nodes_survive_emission() {
    fn owned_boundary<T: Send + Sync>() {}
    owned_boundary::<PreparedWarmHarness>();
    owned_boundary::<WarmHarnessRequest>();
    for anchor in WarmAnchor::ALL {
        let input = request(anchor);
        let plan = recipes::prepare(&input.source).expect("complete recipe");
        let output = prepare_warm_harness(input).expect("full canonical source emits");
        assert_eq!(
            source_sha256(fixture(anchor).as_bytes()),
            anchor.original_sha256()
        );
        assert!(
            output.original_script.ends_with(fixture(anchor)),
            "untimed oracle retains exact entire source"
        );
        assert_eq!(
            source_sha256(output.original_script.as_bytes()),
            output.manifest.original_script_sha256
        );
        for piece in &output.manifest.pieces {
            assert_eq!(
                source_sha256(
                    &fixture(anchor).as_bytes()[piece.start as usize..piece.end as usize]
                ),
                piece.sha256
            );
        }
        ast::parse(&output.script, |program| {
            let work = ast::function(program, "__rfWarmWork")?;
            let body = work.body.as_ref().expect("work body");
            ast::parse(&plan.work, |original| {
                assert_eq!(body.statements.len(), original.body.len());
                for (emitted, original) in body.statements.iter().zip(&original.body) {
                    assert!(
                        emitted.content_eq(original),
                        "{anchor:?} work must preserve original AST"
                    );
                }
                Ok(())
            })
        })
        .expect("generated Script AST");
    }
}

#[test]
fn source_identity_and_required_sampling_cannot_be_relaxed() {
    let mut input = request(WarmAnchor::Fib);
    input.source.original.push_str("console.log(1);");
    assert!(prepare_warm_harness(input).is_err());
    for (warm, measured, iterations) in [(2, 5, None), (3, 4, None), (3, 5, Some(1))] {
        let mut input = request(WarmAnchor::Fib);
        input.sampling.warmup_count = warm;
        input.sampling.sample_count = measured;
        input.sampling.iterations_per_sample = iterations;
        assert!(prepare_warm_harness(input).is_err());
    }
}

#[test]
fn ast_rejects_reduced_work_extra_driver_and_reserved_names() {
    for source in [
        "for(let i=0;i<4;i++)s+=fib(30);console.log(s);",
        "for(let i=0;i<5;i++)s+=fib(30);console.log(s);console.log(1);",
    ] {
        assert!(
            ast::parse(source, |program| ast::tail(
                program,
                "for(let i=0;i<5;i++)s+=fib(30);console.log(s);"
            )
            .map(|_| ()))
            .is_err()
        );
    }
    for source in [
        "var __rfWarmShadow=1;",
        "object.__rfWarmClock;",
        "object['__rfWarmHidden'];",
    ] {
        assert!(ast::parse(source, ast::check_original).is_err());
    }
}

#[test]
fn persistent_reset_and_lifetime_contracts_keep_original_state_checks() {
    let crypto = recipes::prepare(&request(WarmAnchor::Crypto).source).expect("crypto recipe");
    ast::parse(&crypto.reset, |program| {
        assert_eq!(
            program.body.len(),
            6,
            "four clears, original ResetRNG and original pool If"
        );
        assert!(matches!(
            program.body.last(),
            Some(oxc_ast::ast::Statement::IfStatement(_))
        ));
        Ok(())
    })
    .expect("crypto reset AST");
    let typescript = recipes::prepare(&request(WarmAnchor::Typescript).source).expect("TS recipe");
    assert!(typescript.data.contains_key("compiler_input"));
    assert_eq!(typescript.cleanup.trim(), "tearDownTypescript();");
    let zlib = recipes::prepare(&request(WarmAnchor::Zlib).source).expect("zlib recipe");
    assert_eq!(zlib.embedded.len(), 1);
    assert_eq!(zlib.embedded[0].integrity.len(), 1);
    assert!(zlib.cleanup.is_empty());
    assert_eq!(zlib.final_cleanup.trim(), "tearDownZlib();");
    assert_eq!(zlib.required["comparisons"], WarmCheckValue::Integer(720));
    assert_eq!(zlib.required["bytes"], WarmCheckValue::Integer(72000000));
    assert_eq!(
        zlib.required["inputByteSum"],
        WarmCheckValue::Integer(2773014)
    );
    assert!(
        recipes::inspect_zlib("({_memcmp:function(){Module.zlibIntegrityBytes=0;return 0;}});")
            .is_err()
    );
    let earley =
        recipes::prepare(&request(WarmAnchor::EarleyBoyer).source).expect("full Earley recipe");
    assert_eq!(
        earley.contract["completeWork"],
        "2500 Earley + 200 Boyer; original Setup/TearDown and every warn/throw; Earley132"
    );
}

fn records(manifest: &WarmHarnessManifest) -> Vec<WarmRecord> {
    let mut output = vec![WarmRecord::Ready {
        anchor: manifest.anchor,
        original_sha256: manifest.original_sha256.clone(),
        scope: manifest.scope.clone(),
        clock: manifest.clock.clone(),
        warmup_count: 3,
        sample_count: 5,
    }];
    for (phase, count) in [(WarmPhase::Warmup, 3), (WarmPhase::Measured, 5)] {
        for index in 0..count {
            output.push(WarmRecord::Invocation(WarmInvocation {
                phase,
                index,
                elapsed_ns_decimal: (phase == WarmPhase::Measured)
                    .then(|| (9007199254740993u64 + u64::from(index)).to_string()),
                result: manifest.expected_result.clone().expect("known expected"),
                checks: manifest.required_checks.clone(),
            }));
        }
    }
    output.push(WarmRecord::Complete {
        anchor: manifest.anchor,
        warmup_count: 3,
        sample_count: 5,
    });
    output
}
fn stdout(records: &[WarmRecord]) -> Vec<u8> {
    let mut output = String::new();
    for record in records {
        output.push_str(emit::PREFIX);
        output.push_str(&serde_json::to_string(record).expect("record JSON"));
        output.push('\n');
    }
    output.into_bytes()
}

#[test]
fn nanoseconds_survive_beyond_number_integer_precision() {
    let manifest = prepare_warm_harness(request(WarmAnchor::Fib))
        .expect("fib")
        .manifest;
    let output =
        validate_warm_records(&manifest, &stdout(&records(&manifest))).expect("exact observations");
    assert_eq!(output.warmups.len(), 3);
    assert_eq!(output.samples.len(), 5);
    assert_eq!(
        output.measured_ns,
        vec![
            9007199254740993,
            9007199254740994,
            9007199254740995,
            9007199254740996,
            9007199254740997
        ]
    );
    let metric = output.metric().expect("existing Metric owner");
    assert_eq!(
        metric.aggregate.value,
        crate::MetricValue::Integer(9007199254740995)
    );
    assert_eq!(metric.unit, crate::MetricUnit::Nanoseconds);
}

#[test]
fn partial_foreign_duplicate_and_failed_warmups_never_score() {
    let manifest = prepare_warm_harness(request(WarmAnchor::Fib))
        .expect("fib")
        .manifest;
    let good = records(&manifest);
    let mut partial = good.clone();
    partial.pop();
    assert!(validate_warm_records(&manifest, &stdout(&partial)).is_err());
    let mut duplicate = good.clone();
    duplicate[2] = duplicate[1].clone();
    assert!(validate_warm_records(&manifest, &stdout(&duplicate)).is_err());
    let mut foreign = good.clone();
    if let WarmRecord::Ready { anchor, .. } = &mut foreign[0] {
        *anchor = WarmAnchor::AstCtor;
    }
    assert!(validate_warm_records(&manifest, &stdout(&foreign)).is_err());
    let mut failed = good.clone();
    if let WarmRecord::Invocation(observation) = &mut failed[1] {
        observation.result = WarmSemanticResult::Integer(0);
    }
    assert!(validate_warm_records(&manifest, &stdout(&failed)).is_err());
    let mut failed = good.clone();
    if let WarmRecord::Invocation(observation) = &mut failed[1] {
        observation.checks = BTreeMap::new();
    }
    assert!(validate_warm_records(&manifest, &stdout(&failed)).is_err());
    for invalid in ["0", "01", "-1", "1.5", "18446744073709551616"] {
        let mut bad = good.clone();
        if let WarmRecord::Invocation(observation) = &mut bad[4] {
            observation.elapsed_ns_decimal = Some(invalid.into());
        }
        assert!(validate_warm_records(&manifest, &stdout(&bad)).is_err());
    }
    assert!(validate_warm_records(&manifest, b"unexpected console output\n").is_err());
    let mut extra_blank_line = stdout(&good);
    extra_blank_line.push(b'\n');
    assert!(validate_warm_records(&manifest, &extra_blank_line).is_err());
    let crypto = prepare_warm_harness(request(WarmAnchor::Crypto))
        .expect("crypto")
        .manifest;
    assert!(crypto.expected_result.is_none());
    assert!(validate_warm_records(&crypto, b"").is_err());
}
