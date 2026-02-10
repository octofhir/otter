use std::path::Path;

use otter_engine::EngineBuilder;
use tempfile::tempdir;

fn path_literal(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy().to_string()).unwrap()
}

#[test]
fn test_runtime_esm_imports_cjs_default_and_named() {
    let dir = tempdir().unwrap();
    let lib = dir.path().join("lib.cjs");
    std::fs::write(&lib, "module.exports = { named: 7, extra: 9 };").unwrap();

    let mut engine = EngineBuilder::new().build();
    let code = format!(
        r#"
            import cjs, {{ named, extra }} from {};
            if (cjs.named !== 7) throw new Error('default export mismatch');
            if (named !== 7 || extra !== 9) throw new Error('named export mismatch');
            'ok';
        "#,
        path_literal(&lib)
    );

    let result = engine.eval_sync(&code).unwrap();
    assert_eq!(
        result.as_string().map(|s| s.to_string()).as_deref(),
        Some("ok")
    );
}

#[test]
fn test_runtime_cjs_require_esm_namespace_contract() {
    let dir = tempdir().unwrap();
    let esm = dir.path().join("lib.mjs");
    let cjs = dir.path().join("entry.cjs");

    std::fs::write(&esm, "export const named = 22; export default 11;").unwrap();
    std::fs::write(
        &cjs,
        format!(
            "const ns = require({}); module.exports = {{ defaultValue: ns.default, namedValue: ns.named }};",
            path_literal(&esm)
        ),
    )
    .unwrap();

    let mut engine = EngineBuilder::new().build();
    let code = format!(
        r#"
            import {};
            import out from {};
            if (out.defaultValue !== 11) throw new Error('default namespace value mismatch');
            if (out.namedValue !== 22) throw new Error('named namespace value mismatch');
            'ok';
        "#,
        path_literal(&esm),
        path_literal(&cjs)
    );

    let result = engine.eval_sync(&code).unwrap();
    assert_eq!(
        result.as_string().map(|s| s.to_string()).as_deref(),
        Some("ok")
    );
}

#[test]
fn test_runtime_cycle_esm_cjs_foundation() {
    let dir = tempdir().unwrap();
    let a = dir.path().join("a.mjs");
    let b = dir.path().join("b.cjs");

    std::fs::write(
        &a,
        format!(
            "import b from {}; export default {{ fromB: b.fromB, seenA: b.seenA }}; export const marker = 1;",
            path_literal(&b)
        ),
    )
    .unwrap();
    std::fs::write(
        &b,
        format!(
            "const a = require({}); module.exports = {{ fromB: 2, seenA: typeof a === 'object' }};",
            path_literal(&a)
        ),
    )
    .unwrap();

    let mut engine = EngineBuilder::new().build();
    let code = format!(
        r#"
            import {};
            import result from {};
            if (result.fromB !== 2) throw new Error('cycle value mismatch');
            if (result.seenA !== true) throw new Error('cycle namespace mismatch');
            'ok';
        "#,
        path_literal(&b),
        path_literal(&a)
    );

    let result = engine.eval_sync(&code).unwrap();
    assert_eq!(
        result.as_string().map(|s| s.to_string()).as_deref(),
        Some("ok")
    );
}

#[test]
fn test_runtime_cache_identity_between_import_and_require() {
    let dir = tempdir().unwrap();
    let shared = dir.path().join("shared.cjs");
    let bridge = dir.path().join("bridge.cjs");

    std::fs::write(&shared, "module.exports = { value: 1 };").unwrap();
    std::fs::write(
        &bridge,
        format!(
            "const shared = require({}); shared.value += 1; module.exports = shared;",
            path_literal(&shared)
        ),
    )
    .unwrap();

    let mut engine = EngineBuilder::new().build();
    let code = format!(
        r#"
            import shared from {};
            import bridge from {};
            if (shared !== bridge) throw new Error('cache identity mismatch');
            if (shared.value !== 2) throw new Error('cache mutation mismatch');
            'ok';
        "#,
        path_literal(&shared),
        path_literal(&bridge)
    );

    let result = engine.eval_sync(&code).unwrap();
    assert_eq!(
        result.as_string().map(|s| s.to_string()).as_deref(),
        Some("ok")
    );
}
