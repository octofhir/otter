//! Lossless host module requests and observable ImportCall validation order.
//!
//! # Contents
//! - Prelinked and on-demand UTF-8 module identities, including U+FFFD.
//! - Static requests reject unknown keys before loading; dynamic options keep
//!   their distinct TypeError rejection and getter evaluation order.
//! - Coercion, attribute getters, and promise rejection precedence.
//!
//! # Invariants
//! - Lone surrogates never select another module through replacement decoding.
//! - Attribute enumeration completes before type/host validation.

use otter_runtime::{CapabilitySet, Otter, Runtime, SourceInput};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_requests_do_not_alias_replacement_characters() {
    for prelinked in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        for name in ["�", "😀"] {
            std::fs::write(
                dir.path().join(format!("{name}.js")),
                "export const answer = 42;",
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("data.json"), r#"{"answer":42}"#).unwrap();
        let source = format!(
            r#"
            {}
            for (const unit of ['\ud800', '\udc00']) {{
                let calls = 0;
                const specifier = {{ toString() {{ ++calls; return './' + unit + '.js'; }} }};
                const promise = import(specifier);
                if (!(promise instanceof Promise)) throw new Error('not a promise');
                let reason;
                try {{ await promise; }} catch (error) {{ reason = error; }}
                if (!(reason instanceof TypeError) || !reason.message.includes('unpaired surrogate'))
                    throw new Error('malformed request loaded another module: ' + reason);
                if (calls !== 1) throw new Error('coercion repeated');
            }}
            const attrs = new Proxy({{ type: 'xjson'.slice(1), hidden: true }}, {{
                getOwnPropertyDescriptor(target, key) {{
                    if (key === 'hidden') {{
                        for (let i = 0; i < 20; ++i) Array(20).fill('allocation').join('');
                        return {{ configurable: true, enumerable: false, value: true }};
                    }}
                    return Reflect.getOwnPropertyDescriptor(target, key);
                }}
            }});
            const jsonPath = './' + 'data.json';
            const json = await import(jsonPath, {{ with: attrs }});
            if (json.default.answer !== 42) throw new Error('attribute lost across collection');
            for (const name of ['\ufffd', '\ud83d\ude00']) {{
                const path = './' + name + '.js';
                const first = await import(path);
                const second = await import(path);
                if (first.answer !== 42 || first !== second) throw new Error('valid identity changed');
            }}
            "#,
            if prelinked {
                "import './�.js'; import './😀.js';"
            } else {
                ""
            }
        );
        let entry = dir.path().join("entry.js");
        std::fs::write(&entry, source).unwrap();
        let otter = Otter::builder()
            .capabilities(CapabilitySet::allow_all())
            .build()
            .unwrap();
        otter.run_module(&entry).await.unwrap();
    }
}

#[test]
fn import_validation_preserves_getters_and_abrupt_completions() {
    let mut runtime = Runtime::builder().build().unwrap();
    runtime.run_module_source(SourceInput::from_javascript(r#"
        const marker = {};
        let trace = '';
        async function rejected(specifier, options, expected) {
            const promise = import(specifier, options);
            if (!(promise instanceof Promise)) throw new Error('synchronous rejection');
            let reason;
            try { await promise; } catch (error) { reason = error; }
            if (expected === marker ? reason !== marker : !(reason instanceof TypeError))
                throw new Error('wrong rejection: ' + reason);
            return reason;
        }
        // ToString is before options access, but host UTF-8 validation is after it.
        await rejected({ toString() { trace += 's'; throw marker; } },
            { get with() { trace += 'w'; } }, marker);
        if (trace !== 's') throw new Error('coercion order');
        trace = '';
        await rejected({ toString() { trace += 's'; return '\ud800'; } },
            { get with() { trace += 'w'; throw marker; } }, marker);
        if (trace !== 'sw') throw new Error('host validation ran too early');
        // EnumerableOwnProperties must finish before any attribute validation.
        for (const attrs of [
            { get type() { trace += 't'; return 42; }, get later() { trace += 'l'; throw marker; } },
            { get unknown() { trace += 'u'; return 'x'; }, get later() { trace += 'l'; throw marker; } },
            { get type() { trace += 't'; return '\ud800'; }, get later() { trace += 'l'; throw marker; } }
        ]) {
            trace = '';
            await rejected('missing', { with: attrs }, marker);
            if (!['tl', 'ul'].includes(trace)) throw new Error('attribute getter skipped: ' + trace);
        }
        let literalReason;
        try { await import('\ud800'); } catch (error) { literalReason = error; }
        if (!(literalReason instanceof TypeError)
            || !literalReason.message.includes('unpaired surrogate'))
            throw new Error('dynamic literal changed before runtime');
        for (const specifier of ['\ud800', '\udc00']) {
            const reason = await rejected(specifier);
            if (!reason.message.includes('unpaired surrogate')) throw new Error('lossy specifier');
        }
        for (const type of ['\ud800', '\udc00']) {
            const reason = await rejected('missing', { with: { type } });
            if (!reason.message.includes('unpaired surrogate')) throw new Error('lossy attribute: ' + reason);
        }
        // Symbols and non-enumerable properties are not attributes.
        const attrs = { type: '\ud800', get [Symbol()]() { throw marker; } };
        Object.defineProperty(attrs, 'hidden', { get() { throw marker; } });
        const reason = await rejected('missing', { with: attrs });
        if (!reason.message.includes('unpaired surrogate')) throw new Error('filtered attributes');
    "#), "https://example.test/entry.js").unwrap();
}

#[test]
fn static_attributes_reject_unknown_keys_before_loading() {
    use otter_runtime::{DiagnosticKind, OtterError};

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("dep.mjs"),
        "globalThis.depRan = true; export default 1;",
    )
    .unwrap();
    std::fs::write(dir.path().join("data.json"), "1").unwrap();
    let entry = dir.path().join("entry.mjs");
    for declaration in [
        "import './dep.mjs'",
        "import './missing.mjs'",
        "import value from './dep.mjs'",
        "import * as ns from './dep.mjs'",
        "import defer * as ns from './dep.mjs'",
        "export { default } from './dep.mjs'",
        "export * from './dep.mjs'",
        "export * as ns from './dep.mjs'",
    ] {
        for attributes in [
            "extra: 'x'",
            "type: 'json', extra: 'x'",
            "extra: 'x', type: 'json'",
            "'ty\\ud800pe': 'json'",
        ] {
            let declaration = if attributes.contains("type:") {
                declaration.replace("dep.mjs", "data.json")
            } else {
                declaration.to_owned()
            };
            let source =
                format!("{declaration} with {{ {attributes} }}; globalThis.entryRan = true;");
            std::fs::write(&entry, &source).unwrap();
            let mut runtime = Runtime::builder().build().unwrap();
            let error = runtime.run_module(&entry).expect_err(&source);
            let OtterError::Compile { diagnostics } = error else {
                panic!("{error:?}")
            };
            assert!(
                diagnostics.iter().any(|d| d.kind == DiagnosticKind::Syntax
                    && d.message.contains("unsupported import attribute")),
                "{source}: {diagnostics:?}"
            );
            assert_eq!(
                runtime
                    .run_script(
                        SourceInput::from_javascript("typeof depRan + ':' + typeof entryRan"),
                        "side-effects.mjs"
                    )
                    .unwrap()
                    .completion_string(),
                "undefined:undefined"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn importing_invalid_static_attributes_rejects_with_syntax_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("dep.mjs"),
        "globalThis.depRan = true; export default 1;",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("invalid.mjs"),
        "import './dep.mjs' with { extra: 'x' }; globalThis.invalidRan = true;",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("literal-invalid.mjs"),
        "export * from './dep.mjs' with { extra: 'x' };",
    )
    .unwrap();
    std::fs::write(dir.path().join("data.json"), "42").unwrap();
    let entry = dir.path().join("entry.mjs");
    std::fs::write(&entry, r#"
        const invalid = './' + 'invalid.mjs';
        let reason;
        try { await import(invalid); } catch (error) { reason = error; }
        if (!(reason instanceof SyntaxError) || !reason.message.includes('unsupported import attribute'))
            throw new Error('static request: ' + reason);
        if (typeof depRan !== 'undefined' || typeof invalidRan !== 'undefined')
            throw new Error('rejected graph executed');
        reason = undefined;
        try { await import('./literal-invalid.mjs'); } catch (error) { reason = error; }
        if (!(reason instanceof SyntaxError) || !reason.message.includes('unsupported import attribute'))
            throw new Error('preloaded static request: ' + reason);
        const path = './' + 'dep.mjs';
        reason = undefined;
        try { await import(path, { with: { extra: 'x' } }); } catch (error) { reason = error; }
        if (!(reason instanceof TypeError)) throw new Error('dynamic options: ' + reason);
        if (typeof depRan !== 'undefined') throw new Error('rejected import executed');
        const { default: valid } = await import('./data.json', { with: { 't\u0079pe': 'json' } });
        if (valid !== 42) throw new Error('supported key changed');
        globalThis.attributesChecked = true;
    "#).unwrap();
    let otter = Otter::builder()
        .capabilities(CapabilitySet::allow_all())
        .build()
        .unwrap();
    otter.run_module(&entry).await.unwrap();
    let error = otter
        .run_module(dir.path().join("invalid.mjs"))
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("unsupported import attribute"));
    assert_eq!(
        otter
            .eval("globalThis.attributesChecked")
            .await
            .unwrap()
            .completion_string(),
        "true"
    );
}
