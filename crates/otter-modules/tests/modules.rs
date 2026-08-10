use std::collections::BTreeMap;

use otter_modules::ffi::{FfiSignature, FfiType};
use otter_modules::kv::KvStore;
use otter_modules::sql::SqlDatabase;
use otter_modules::{OtterModulesBuilderExt, hosted_modules};
use otter_runtime::{CapabilitySet, Permission, Runtime};
use serde_json::json;

#[test]
fn hosted_module_specs_are_static_and_ordered() {
    let specs = hosted_modules();
    assert_eq!(specs.len(), 4);
    assert_eq!(specs[0].specifier(), "otter");
    assert_eq!(specs[1].specifier(), "otter:kv");
    assert_eq!(specs[2].specifier(), "otter:sql");
    assert_eq!(specs[3].specifier(), "otter:ffi");
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

    let mut runtime = Runtime::builder().with_otter_modules().build().unwrap();
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

    let mut runtime = Runtime::builder().with_otter_modules().build().unwrap();
    runtime.run_module(&main).unwrap();
}

#[test]
fn bare_otter_module_exports_serve() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
            import { serve } from "otter";
            if (typeof serve !== "function") {
                throw new Error("serve export missing");
            }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder().with_otter_modules().build().unwrap();
    runtime.run_module(&main).unwrap();
}

#[test]
fn otter_global_installs_serve() {
    let mut runtime = Runtime::builder().with_otter_modules().build().unwrap();
    runtime
        .eval(otter_runtime::SourceInput::from_javascript(
            r#"
            if (typeof Otter !== "object") throw new Error("Otter global missing");
            if (typeof Otter.serve !== "function") throw new Error("Otter.serve missing");
            "#,
        ))
        .unwrap();
}

#[test]
fn hosted_namespace_is_cached_across_runs_and_loaders() {
    let dir = tempfile::tempdir().unwrap();

    // Run 1 (ESM): stamp an expando on the `otter:kv` namespace object.
    let first = dir.path().join("first.mjs");
    std::fs::write(
        &first,
        r#"
            import * as kv from "otter:kv";
            kv.openKv(":memory:");
        "#,
    )
    .unwrap();

    // Run 2 (ESM, same runtime): the namespace must be the same installed
    // object, not a fresh install.
    let second = dir.path().join("second.mjs");
    std::fs::write(
        &second,
        r#"
            import { openKv } from "otter:kv";
            if (typeof openKv !== "function") {
                throw new Error("cached namespace lost openKv");
            }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder().with_otter_modules().build().unwrap();
    runtime.run_module(&first).unwrap();
    runtime.run_module(&second).unwrap();
}

#[test]
fn duplicate_hosted_specifier_is_a_build_error() {
    let err = Runtime::builder()
        .with_otter_modules()
        .hosted_modules(hosted_modules().iter().copied())
        .build()
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("registered more than once"),
        "unexpected error: {message}"
    );
}

#[test]
fn otter_xml_parses_both_shapes_and_reports_bad_documents() {
    let mut runtime = Runtime::builder().with_otter_modules().build().unwrap();
    runtime
        .eval(otter_runtime::SourceInput::from_javascript(
            r##"
            function check(actual, expected, what) {
              const a = JSON.stringify(actual);
              const e = JSON.stringify(expected);
              if (a !== e) throw new Error(what + ": " + a + " !== " + e);
            }
            if (typeof Otter.XML.parse !== "function") throw new Error("Otter.XML.parse missing");

            check(
              Otter.XML.parse(
                "<order id='A1'><customer>Ada</customer>" +
                "<item sku='tea'>Green tea</item><item sku='mug'>Mug</item><paid/></order>"
              ),
              { order: {
                  "@id": "A1",
                  customer: "Ada",
                  item: [
                    { "@sku": "tea", "#text": "Green tea" },
                    { "@sku": "mug", "#text": "Mug" },
                  ],
                  paid: "",
              } },
              "compact"
            );

            check(
              Otter.XML.parse("<p class='lead'>Hello <b>world</b>!</p>", { compact: false }),
              { name: "p", attributes: { class: "lead" }, children: [
                  "Hello ",
                  { name: "b", attributes: {}, children: ["world"] },
                  "!",
              ] },
              "node"
            );

            // Entities, CDATA and line ends are resolved before the value is built.
            check(
              Otter.XML.parse("<a>&lt;&#65;<![CDATA[<raw>]]>x\r\ny</a>"),
              { a: "<A<raw>x\ny" },
              "text"
            );

            // Bytes are decoded per the declaration, not assumed to be UTF-8.
            const ascii = (s) => Array.from(s, (c) => c.charCodeAt(0));
            const latin1 = new Uint8Array([
              ...ascii("<?xml version='1.0' encoding='ISO-8859-1'?><a b='"),
              0xE9,
              ...ascii("'/>"),
            ]);
            check(Otter.XML.parse(latin1), { a: { "@b": "é" } }, "latin-1 bytes");

            let threw = null;
            try { Otter.XML.parse("<a><b></a>"); } catch (error) { threw = error; }
            if (!(threw instanceof SyntaxError)) throw new Error("expected a SyntaxError");
            if (!threw.message.includes("</b>")) throw new Error("message: " + threw.message);

            try { Otter.XML.parse(42); threw = null; } catch (error) { threw = error; }
            if (!(threw instanceof TypeError)) throw new Error("expected a TypeError");
            "##,
        ))
        .unwrap();
}

#[test]
fn otter_xml_keeps_keys_when_elements_of_one_name_differ() {
    // An element is built with the hidden class the last element of its name
    // closed with. Every way the next one can diverge from that class has to
    // yield the same object as if nothing had been remembered.
    let mut runtime = Runtime::builder().with_otter_modules().build().unwrap();
    runtime
        .eval(otter_runtime::SourceInput::from_javascript(
            r##"
            function check(actual, expected, what) {
              const a = JSON.stringify(actual);
              const e = JSON.stringify(expected);
              if (a !== e) throw new Error(what + ": " + a + " !== " + e);
            }

            // A later element takes a key the earlier one did not, takes them
            // in another order, takes fewer, and takes none at all.
            check(
              Otter.XML.parse(
                "<r>" +
                "<a x='1' y='2'/><a x='3' y='4'/>" +
                "<a x='5' y='6' z='7'/><a y='8' x='9'/><a x='0'/><a>text</a>" +
                "</r>"
              ),
              { r: { a: [
                  { "@x": "1", "@y": "2" },
                  { "@x": "3", "@y": "4" },
                  { "@x": "5", "@y": "6", "@z": "7" },
                  { "@y": "8", "@x": "9" },
                  { "@x": "0" },
                  "text",
              ] } },
              "diverging attributes"
            );

            // Repeating a child name is one key holding a list, however many
            // times it repeats and whichever element of the name comes first.
            check(
              Otter.XML.parse(
                "<r><e><c>1</c></e><e><c>2</c><c>3</c><c>4</c></e><e><d>5</d><c>6</c></e></r>"
              ),
              { r: { e: [
                  { c: "1" },
                  { c: ["2", "3", "4"] },
                  { d: "5", c: "6" },
              ] } },
              "repeated children"
            );

            // Text joins the keys last, so an element that gains or loses it
            // diverges at the end of the class rather than the middle.
            check(
              Otter.XML.parse("<r><v k='a'>one</v><v k='b'></v><v k='c'>two</v></r>"),
              { r: { v: [
                  { "@k": "a", "#text": "one" },
                  { "@k": "b" },
                  { "@k": "c", "#text": "two" },
              ] } },
              "text as a key"
            );

            // Enough divergence stops the speculation; the elements after it
            // are still built correctly.
            let parts = [];
            for (let i = 0; i < 12; i++) {
              parts.push(i % 2 === 0 ? "<a x='" + i + "'/>" : "<a y='" + i + "'/>");
            }
            const alternating = Otter.XML.parse("<r>" + parts.join("") + "</r>").r.a;
            if (alternating.length !== 12) throw new Error("length " + alternating.length);
            for (let i = 0; i < 12; i++) {
              const expected = i % 2 === 0 ? { "@x": String(i) } : { "@y": String(i) };
              check(alternating[i], expected, "alternating " + i);
            }
            "##,
        ))
        .unwrap();
}
