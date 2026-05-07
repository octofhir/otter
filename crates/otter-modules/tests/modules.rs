use std::collections::BTreeMap;

use otter_modules::ffi::{FfiSignature, FfiType};
use otter_modules::hosted_modules;
use otter_modules::kv::KvStore;
use otter_modules::sql::SqlDatabase;
use otter_runtime::{CapabilitySet, Permission, Runtime};
use serde_json::json;

#[test]
fn hosted_module_specs_are_static_and_ordered() {
    let specs = hosted_modules();
    assert_eq!(specs.len(), 3);
    assert_eq!(specs[0].specifier, "otter:kv");
    assert_eq!(specs[1].specifier, "otter:sql");
    assert_eq!(specs[2].specifier, "otter:ffi");
}

#[test]
fn kv_memory_round_trips_deterministically() {
    let mut store = KvStore::memory();
    store.set("b", json!(2)).unwrap();
    store.set("a", json!({"nested": true})).unwrap();
    assert_eq!(store.get("b"), Some(json!(2)));
    assert_eq!(store.keys(), vec!["a".to_string(), "b".to_string()]);
    assert!(store.delete("a").unwrap());
    assert!(!store.has("a"));
}

#[test]
fn kv_file_open_requires_write_permission() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.json");
    let caps = CapabilitySet {
        read: Permission::allow([path.clone()]),
        write: Permission::Deny,
        ..CapabilitySet::sandbox()
    };
    let err = KvStore::open(&path, &caps).unwrap_err();
    assert!(err.to_string().contains("permission denied"));
}

#[test]
fn sql_memory_queries_json_rows() {
    let mut db = SqlDatabase::memory().unwrap();
    db.execute("CREATE TABLE users (id INTEGER, name TEXT)", &[])
        .unwrap();
    db.execute("INSERT INTO users VALUES (?, ?)", &[json!(1), json!("Ada")])
        .unwrap();
    let rows = db
        .query("SELECT id, name FROM users WHERE id = ?", &[json!(1)])
        .unwrap();
    assert_eq!(rows, vec![json!({"id": 1, "name": "Ada"})]);
}

#[test]
fn ffi_signature_parses_known_types() {
    let signature = FfiSignature::parse(&["cstring", "i32"], "void").unwrap();
    assert_eq!(signature.args, vec![FfiType::CString, FfiType::I32]);
    assert_eq!(signature.returns, FfiType::Void);
    assert!(FfiType::parse("unknown").is_err());
    let _: BTreeMap<String, FfiSignature> = BTreeMap::new();
}

#[test]
fn otter_kv_resolves_and_runs_from_module_graph() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
            import { openKv } from "otter:kv";
            const store = openKv(":memory:");
            store.set("answer", "forty-two");
            if (store.get("answer") !== "forty-two") {
                throw new Error("kv get failed");
            }
            if (!store.has("answer")) {
                throw new Error("kv has failed");
            }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .hosted_modules(hosted_modules().iter().copied())
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

#[test]
fn otter_sql_resolves_and_runs_from_module_graph() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
            import { openSql } from "otter:sql";
            const db = openSql(":memory:");
            db.execute("CREATE TABLE t (id INTEGER, name TEXT)");
            db.execute("INSERT INTO t VALUES (?, ?)", 7, "seven");
            const rows = db.query("SELECT name FROM t WHERE id = ?", 7);
            if (rows[0].name !== "seven") {
                throw new Error("sql query failed");
            }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .hosted_modules(hosted_modules().iter().copied())
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}
