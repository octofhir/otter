//! Frontmatter + feature_map integration tests for slice 102.
//!
//! Spec: <https://github.com/tc39/test262/blob/main/INTERPRETING.md#metadata>

use otter_test262::feature_map::{FeatureMap, Readiness};
use otter_test262::metadata::{Frontmatter, FrontmatterError, NegativePhase, TestFlag};

#[test]
fn parses_every_flag_combination() {
    // Every legal token from INTERPRETING §flags.
    let combos: &[(&str, TestFlag)] = &[
        ("onlyStrict", TestFlag::OnlyStrict),
        ("noStrict", TestFlag::NoStrict),
        ("module", TestFlag::Module),
        ("raw", TestFlag::Raw),
        ("async", TestFlag::Async),
        ("generated", TestFlag::Generated),
        ("CanBlockIsFalse", TestFlag::CanBlockIsFalse),
        ("CanBlockIsTrue", TestFlag::CanBlockIsTrue),
        ("non-deterministic", TestFlag::NonDeterministic),
    ];
    for (token, want) in combos {
        let src = format!("/*---\nflags: [{token}]\n---*/\n");
        let fm = Frontmatter::parse(&src).expect("legal flag should parse");
        let typed = fm.test_flags();
        assert!(
            typed.contains(want),
            "token {token} did not produce {want:?}"
        );
    }

    // Every flag in one block.
    let all = "/*---\nflags: [onlyStrict, noStrict, module, raw, async, generated, CanBlockIsFalse, CanBlockIsTrue, non-deterministic]\n---*/\n";
    let fm = Frontmatter::parse(all).unwrap();
    assert_eq!(fm.test_flags().len(), 9);
}

#[test]
fn parses_every_negative_phase() {
    for (phase_token, want) in [
        ("parse", NegativePhase::Parse),
        ("early", NegativePhase::Early),
        ("resolution", NegativePhase::Resolution),
        ("runtime", NegativePhase::Runtime),
    ] {
        let src = format!("/*---\nnegative:\n  phase: {phase_token}\n  type: SyntaxError\n---*/\n");
        let fm = Frontmatter::parse(&src).expect("legal phase should parse");
        let neg = fm.negative.expect("negative present");
        assert_eq!(neg.phase, want);
        assert_eq!(neg.type_, "SyntaxError");
    }
}

#[test]
fn early_canonicalises_to_parse() {
    let src = "/*---\nnegative:\n  phase: early\n  type: SyntaxError\n---*/\n";
    let fm = Frontmatter::parse(src).unwrap();
    assert_eq!(fm.negative.unwrap().phase.canonical(), NegativePhase::Parse);
}

#[test]
fn parses_multiline_info_block() {
    let src = "// header\n/*---\ndescription: x\nesid: sec-foo\ninfo: |\n  Some\n  multi-line\n  body.\n  Another paragraph.\nfeatures: [Symbol]\n---*/\n1;\n";
    let fm = Frontmatter::parse(src).unwrap();
    let info = fm.info.expect("info present");
    assert!(info.contains("multi-line"));
    assert!(info.contains("Another paragraph"));
}

#[test]
fn parses_includes_in_order() {
    let src = "/*---\nincludes: [propertyHelper.js, compareArray.js, sta.js]\n---*/\n";
    let fm = Frontmatter::parse(src).unwrap();
    assert_eq!(
        fm.includes,
        vec!["propertyHelper.js", "compareArray.js", "sta.js"]
    );
}

#[test]
fn handles_missing_trailing_newline_without_panic() {
    // Block closes without a newline before `---*/` and no trailing
    // newline at EOF. The corpus does carry tests like this and we
    // must never panic.
    let src = "/*---\ndescription: ok---*/";
    let _ = Frontmatter::parse(src); // either Ok or Yaml(...)
}

#[test]
fn missing_block_surfaces_error() {
    let err = Frontmatter::parse("// no frontmatter").unwrap_err();
    assert!(matches!(err, FrontmatterError::MissingBlock));
}

#[test]
fn unclosed_block_surfaces_error() {
    let err = Frontmatter::parse("/*---\ndescription: x").unwrap_err();
    assert!(matches!(err, FrontmatterError::UnclosedBlock));
}

#[test]
fn malformed_yaml_surfaces_yaml_error() {
    // A YAML mapping value cannot start with `[`. Attempting to use
    // an unbracketed alias-like token forces a parser error.
    let src = "/*---\nflags: !!nonsense {\n---*/\n";
    let res = Frontmatter::parse(src);
    assert!(matches!(res, Err(FrontmatterError::Yaml(_))));
}

#[test]
fn body_of_returns_post_block_text() {
    let src = "/*---\ndescription: x\n---*/\nlet a = 1;\n";
    assert_eq!(Frontmatter::body_of(src), "\nlet a = 1;\n");

    // No frontmatter: body_of returns the whole source.
    assert_eq!(Frontmatter::body_of("plain"), "plain");
}

#[test]
fn feature_map_grades_when_token_absent_from_skip_list() {
    // Empty config → every feature grades.
    let map = FeatureMap::default();
    assert_eq!(map.lookup("BigInt"), Readiness::Grade);
    assert_eq!(map.lookup("Atomics"), Readiness::Grade);
}

#[test]
fn feature_map_skips_tokens_listed_in_config() {
    // Config-driven: only the tokens passed in are "skip".
    let map = FeatureMap::from_skip_features(["Atomics", "ShadowRealm"]);
    assert_eq!(map.lookup("Atomics"), Readiness::Skip);
    assert_eq!(map.lookup("ShadowRealm"), Readiness::Skip);
    assert_eq!(map.lookup("BigInt"), Readiness::Grade);
}

#[test]
fn feature_map_first_skipped_short_circuits() {
    let map = FeatureMap::from_skip_features(["Atomics", "Temporal"]);
    let want_skip = vec![
        "BigInt".to_string(),
        "Temporal".to_string(),
        "class".to_string(),
    ];
    assert_eq!(map.first_skipped(&want_skip), Some("Temporal"));

    let none_in_skip = vec!["BigInt".to_string(), "class".to_string()];
    assert_eq!(map.first_skipped(&none_in_skip), None);
}

#[test]
fn parses_real_test262_frontmatter_shape() {
    // Real shape pulled from
    // vendor/test262/test/built-ins/Array/from/proto-from-ctor-realm.js
    // (verbatim modulo the body, which we replace with `1;`).
    let src = r#"// Copyright (C) 2016 the V8 project authors. All rights reserved.
// This code is governed by the BSD license found in the LICENSE file.
/*---
esid: sec-array.from
es6id: 22.1.2.1
description: Default [[Prototype]] value derived from realm of the constructor
info: |
    [...]
    5. If usingIterator is not undefined, then
       a. If IsConstructor(C) is true, then
          i. Let A be ? Construct(C).
    [...]
features: [cross-realm]
---*/
1;
"#;
    let fm = Frontmatter::parse(src).expect("real-corpus frontmatter parses");
    assert_eq!(fm.esid.as_deref(), Some("sec-array.from"));
    assert_eq!(fm.es6id.as_deref(), Some("22.1.2.1"));
    assert_eq!(fm.features, vec!["cross-realm"]);
    assert!(fm.info.unwrap().contains("usingIterator"));
}
