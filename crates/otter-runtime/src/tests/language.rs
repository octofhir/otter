//! Observable script and builtin behavior.
//!
//! # Contents
//! Regression tests through the runtime embedding API.
//!
//! # Invariants
//! Each test constructs its own runtime.
//!
//! # See also
//! - [`crate::Runtime`] owns the isolate session.

use super::*;

#[test]
fn otter_runs_empty_typescript() {
    let otter = Otter::new();
    let result = otter.blocking_run_typescript("").unwrap();
    assert_eq!(result.completion_string(), "undefined");
}

#[test]
fn otter_runs_undefined_literal() {
    let otter = Otter::new();
    let result = otter.blocking_run_typescript("undefined;").unwrap();
    assert_eq!(result.completion_string(), "undefined");
}

#[test]
fn json_cyclic_throws_type_error() {
    let otter = Otter::new();
    let err = otter
        .blocking_run_typescript("const a = {}; a.self = a; JSON.stringify(a);")
        .unwrap_err();
    match err {
        OtterError::Runtime { diagnostic } => {
            assert_eq!(diagnostic.code, "UNCAUGHT");
            assert!(
                diagnostic.message.contains("TypeError")
                    && diagnostic
                        .message
                        .contains("JSON.stringify cannot serialize cyclic structures."),
                "unexpected diagnostic: {}",
                diagnostic.message,
            );
        }
        other => panic!("expected Runtime, got {other:?}"),
    }
}

#[test]
fn json_parse_error_throws_syntax_error_with_byte_position() {
    let otter = Otter::new();
    let err = otter
        .blocking_run_typescript("JSON.parse(\"[1, 2,]\");")
        .unwrap_err();
    match err {
        OtterError::Runtime { diagnostic } => {
            assert_eq!(diagnostic.code, "UNCAUGHT");
            assert!(
                diagnostic.message.contains("SyntaxError")
                    && diagnostic.message.contains("trailing comma")
                    && diagnostic.message.contains("at byte 6"),
                "unexpected diagnostic: {}",
                diagnostic.message,
            );
        }
        other => panic!("expected Runtime, got {other:?}"),
    }
}

#[test]
fn string_unicode_case_mapping() {
    let otter = Otter::new();
    otter
        .blocking_run_typescript(
            r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + JSON.stringify(a)); }
                eq("Éab".toLowerCase(), "éab", "latin-1");
                eq("ß".toUpperCase(), "SS", "sharp-s-upper");
                eq("İ".toLowerCase(), "i̇", "dotted-I");
                eq("𐐀".toLowerCase(), "𐐨", "supplementary-deseret");
                // Final Sigma: cased before, end-of-word -> final form.
                eq("AΣ".toLowerCase(), "aς", "final-sigma");
                eq("ΣA".toLowerCase(), "σa", "non-final-sigma");
                // toLocale* default to the locale-insensitive mapping.
                eq("ABC".toLocaleLowerCase(), "abc", "locale-lower");
                "#,
        )
        .expect("Unicode case mapping");
}

#[test]
fn eval_directive_prologue_completion_value() {
    let otter = Otter::new();
    otter
        .blocking_run_typescript(
            r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + JSON.stringify(a)); }
                // A directive-prologue string is an expression statement;
                // its value is the eval completion value.
                eq(eval('"hello"'), "hello", "bare-directive");
                eq(eval('"use strict"'), "use strict", "use-strict-value");
                eq(eval('"a"; 1'), 1, "directive-then-stmt");
                "#,
        )
        .expect("eval directive completion value");
}

#[test]
fn regexp_subclass_overrides_and_super_computed_call() {
    let otter = Otter::new();
    otter
        .blocking_run_typescript(
            r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + a); }

                // A RegExp subclass's own method shadows the base
                // prototype via the instance's real [[Prototype]].
                class RE extends RegExp {
                    exec() { return "OWN"; }
                    [Symbol.replace]() { return "SYM"; }
                }
                const r = new RE("b", "g");
                eq(r.exec("xb"), "OWN", "subclass-exec-override");
                eq(r[Symbol.replace]("ab", "z"), "SYM", "subclass-symbol-override");
                // Plain RegExp keeps intrinsic behaviour.
                eq(/b/.exec("ab")[0], "b", "plain-exec");
                eq(/a/gi.flags, "gi", "plain-flags");

                // super[computed](...) resolves the parent method with
                // `this` bound to the receiver (spread + non-spread).
                let called = 0;
                class RE2 extends RegExp {
                    [Symbol.replace](...args) {
                        const out = super[Symbol.replace](...args);
                        called += 1;
                        return out;
                    }
                }
                eq("a b a".replaceAll(new RE2(" ", "g"), "_"), "a_b_a", "super-computed-spread");
                eq(called, 1, "super-computed-called-once");
                "#,
        )
        .expect("RegExp subclass overrides + super[computed] call");
}

#[test]
fn string_replace_split_match_spec_dispatch() {
    let otter = Otter::new();
    otter
        .blocking_run_typescript(
            r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + a); }

                // searchValue coerces via ToString; function replacer fires.
                eq("gnulluna".replace(null, (m, p) => p + ""), "g1una", "replace-null-fn");
                // $-substitution patterns.
                eq("abc".replace("b", "[$&]"), "a[b]c", "replace-dollar-amp");
                eq("abc".replace("b", "$`|$'"), "aa|cc", "replace-dollar-ctx");
                eq("a$b".replace("$", "$$"), "a$b", "replace-dollar-dollar");
                // replaceAll over all occurrences.
                eq("a.b.c".replaceAll(".", "-"), "a-b-c", "replaceAll");

                // split delegates to RegExp @@split (captures included).
                eq("a1b2c".split(/(\d)/).join(","), "a,1,b,2,c", "split-regexp");
                eq("a,b,c".split(",").length, 3, "split-string");

                // match / search delegate to RegExp @@match / @@search.
                eq("a1b2".match(/\d/g).join(""), "12", "match-global");
                eq("xxabc".search(/abc/), 2, "search");
                eq([..."a1b2".matchAll(/\d/g)].length, 2, "matchAll");

                // Primitive searchValue never probes @@replace.
                Object.defineProperty(String.prototype, Symbol.replace, {
                    configurable: true,
                    get() { throw new Error("@@replace probed on primitive"); },
                });
                eq("a,b,c".replace(",", "X"), "aXb,c", "replace-primitive-no-symbol");
                delete String.prototype[Symbol.replace];
                "#,
        )
        .expect("String replace/split/match spec dispatch");
}

#[test]
fn json_parse_reviver_raw_json_and_source() {
    let otter = Otter::new();
    otter
        .blocking_run_typescript(
            r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + a); }

                // Reviver transforms values bottom-up; parsed objects
                // inherit Object.prototype.
                var parsed = JSON.parse('{"a":1,"b":2}', (k, v) =>
                    typeof v === "number" ? v + 1 : v);
                eq(parsed.a, 2, "reviver-a");
                eq(parsed.b, 3, "reviver-b");
                eq(Object.getPrototypeOf(JSON.parse("{}")), Object.prototype, "parse-proto");
                eq(JSON.parse('{"__proto__":1}').__proto__, 1, "parse-proto-own");
                eq(Object.is(JSON.parse("-0"), -0), true, "parse-neg-zero");

                // rawJSON / isRawJSON round-trip.
                var raw = JSON.rawJSON("1.50");
                eq(JSON.isRawJSON(raw), true, "isRawJSON");
                eq(JSON.isRawJSON({}), false, "isRawJSON-plain");
                eq(raw.rawJSON, "1.50", "raw-text");
                eq(Object.isFrozen(raw), true, "raw-frozen");
                eq(JSON.stringify({ x: raw }), '{"x":1.50}', "stringify-raw");

                // Reviver context.source carries the verbatim leaf text.
                var src = JSON.parse("1.50", (k, v, ctx) => ctx.source);
                eq(src, "1.50", "context-source");
                "#,
        )
        .expect("JSON.parse reviver / rawJSON / context.source");
}

#[test]
fn json_stringify_runs_spec_observable_hooks() {
    // toJSON, replacer function, array-replacer PropertyList,
    // wrapper-object ToNumber/ToString unwrap, and verbatim
    // propagation of a user exception thrown from a getter.
    let otter = Otter::new();
    otter
        .blocking_run_typescript(
            r#"
                function eq(a, b, m) { if (a !== b) throw new Error(m + ": " + a); }

                eq(JSON.stringify({ d: { toJSON() { return "X"; } } }),
                   '{"d":"X"}', "toJSON");

                eq(JSON.stringify({ a: 1, b: 2 }, (k, v) => typeof v === "number" ? v * 10 : v),
                   '{"a":10,"b":20}', "replacer-fn");

                eq(JSON.stringify({ a: 1, b: 2, c: 3 }, ["b", "a", "b"]),
                   '{"b":2,"a":1}', "replacer-array");

                eq(JSON.stringify(new Number(8.5)), "8.5", "number-wrapper");
                eq(JSON.stringify(new String("s")), '"s"', "string-wrapper");

                var sw = new String("raw");
                sw.toString = function () { return "cooked"; };
                eq(JSON.stringify(sw), '"cooked"', "string-wrapper-tostring");
                "#,
        )
        .expect("spec-observable JSON.stringify hooks");

    // A user exception thrown from a getter propagates verbatim,
    // not as a wrapped TypeError — the caught value is the exact
    // object that was thrown.
    let err = otter.blocking_run_typescript(
        "var sentinel = { marker: 42 }; \
             try { JSON.stringify({ get k() { throw sentinel; } }); \
             throw new Error('no throw'); } \
             catch (e) { if (e !== sentinel) throw new Error('not verbatim'); }",
    );
    assert!(
        err.is_ok(),
        "user exception should propagate verbatim: {err:?}"
    );
}

#[test]
fn json_bigint_throws_type_error() {
    let otter = Otter::new();
    let err = otter
        .blocking_run_typescript("JSON.stringify({ n: 1n });")
        .unwrap_err();
    match err {
        OtterError::Runtime { diagnostic } => {
            assert_eq!(diagnostic.code, "UNCAUGHT");
            assert!(
                diagnostic.message.contains("TypeError")
                    && diagnostic
                        .message
                        .contains("JSON.stringify cannot serialize BigInt values."),
                "unexpected diagnostic: {}",
                diagnostic.message,
            );
        }
        other => panic!("expected Runtime, got {other:?}"),
    }
}

#[test]
fn otter_rejects_unsupported_js_feature() {
    // `with` in strict mode is a SyntaxError (§14.13) — a stable
    // canary for the structured Compile diagnostic shape.
    let otter = Otter::new();
    let err = otter
        .blocking_run_typescript("\"use strict\"; with (o) { x; }")
        .unwrap_err();
    match err {
        OtterError::Compile { diagnostics } => {
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].code, "STRICT_MODE_WITH");
        }
        other => panic!("expected Compile, got {other:?}"),
    }
}

#[test]
fn otter_rejects_typescript_enum() {
    // `enum` is intentionally rejected by the frontend policy.
    let otter = Otter::new();
    let err = otter.blocking_run_typescript("enum E { A }").unwrap_err();
    match err {
        OtterError::Compile { diagnostics } => {
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].code, "TS_UNSUPPORTED");
        }
        other => panic!("expected Compile, got {other:?}"),
    }
}

#[test]
fn otter_erases_interface() {
    let otter = Otter::new();
    let result = otter
        .blocking_run_typescript("interface I { x: number; } undefined;")
        .unwrap();
    assert_eq!(result.completion_string(), "undefined");
}

#[tokio::test]
async fn commonjs_package_type_forces_script_parse_for_ambiguous_js() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("package.json"), r#"{"type":"commonjs"}"#).unwrap();
    std::fs::write(app.join("dep.js"), "export let value = 1;\n").unwrap();
    std::fs::write(
        app.join("entry.js"),
        "import { value } from \"./dep.js\";\nvalue;\n",
    )
    .unwrap();

    let mut loader = module_loader::LoaderConfig::new(app.clone());
    loader.enable_node_modules = false;
    let otter = Otter::builder().module_loader(loader).build().unwrap();

    let err = otter
        .check_file(app.join("entry.js"))
        .await
        .expect_err("commonjs package type should parse ambiguous .js as script");
    assert!(matches!(err, OtterError::Compile { .. }));
}

#[test]
fn run_file_rejects_unknown_extension() {
    let otter = Otter::new();
    let err = otter.blocking_run_file("/nonexistent.foo").unwrap_err();
    assert!(matches!(err, OtterError::SourceKind { .. }));
}

#[test]
fn json_error_carries_error_payload() {
    let err = OtterError::SourceKind {
        path: PathBuf::from("nope.foo"),
        extension: "foo".to_string(),
    };
    let json = err.to_json().unwrap();
    assert!(json.contains("\"kind\":\"source_kind\""));
}
