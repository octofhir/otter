//! OXC UTF-16 lowering through the public runtime boundary.
//!
//! # Contents
//! - Cooked/raw template units, invalid escapes, frozen arrays and site identity.
//! - Host module text validation, separate from JavaScript string values.
//!
//! # Invariants
//! - Surrogates never alias U+FFFD or literal escape spellings.
//! - `property_key_code_units` covers the object-key matrix separately.

use otter_runtime::{Runtime, SourceInput};

#[test]
fn templates_preserve_utf16_and_site_identity() {
    let mut runtime = Runtime::builder().build().unwrap();
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
        let previous;
        function tag(strings) {
            if (strings[0] !== '\ud800\ufffd' || strings[0].charCodeAt(0) !== 55296
                || strings.raw[0] !== '\\ud800\\ufffd'
                || !Object.isFrozen(strings) || !Object.isFrozen(strings.raw)
                || (previous && previous !== strings)) throw new Error('template changed');
            previous = strings;
        }
        for (let i = 0; i < 1000; ++i) tag`\ud800\ufffd`;
        const literal = '\ud800\ufffdd800\udc00😀';
        if (`\ud800${'\ufffdd800'}\udc00😀` !== literal || literal.length !== 9)
            throw new Error('untagged template changed');
        function invalid(strings) {
            if (strings[0] !== undefined || strings.raw[0] !== '\\u{no}')
                throw new Error('invalid escape changed');
        }
        invalid`\u{no}`;
        let conflict = false;
        try { (function(a = eval('var a; "\ud800"')) {})(); }
        catch (error) { conflict = error instanceof SyntaxError; }
        if (!conflict || eval('"\\ud800"').charCodeAt(0) !== 55296)
            throw new Error('eval changed');
        'ok';
    "#,
            ),
            "<oxc-templates>",
        )
        .unwrap();
    assert_eq!(result.completion_string(), "ok");
}

#[test]
fn host_module_requests_reject_surrogates() {
    let mut runtime = Runtime::builder().build().unwrap();
    for source in [
        r#"import '\ud800';"#,
        r#"export * from '\ud800';"#,
        r#"import x from './x' with { type: '\ud800' };"#,
    ] {
        let error = runtime
            .run_module_source(
                SourceInput::from_javascript(source),
                "https://example.test/entry.js",
            )
            .expect_err("host request is not UTF-8");
        assert!(
            format!("{error:?}").contains("unpaired surrogate"),
            "{error:?}"
        );
    }
}
