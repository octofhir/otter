//! Importing an `.xml` file: the compact shape as the default export, and the
//! root element under its own name.

use otter_runtime::Runtime;

#[test]
fn an_xml_file_imports_as_its_compact_shape() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("feed.xml"),
        r#"<?xml version="1.0"?>
           <feed count="2">
             <entry id="1">first</entry>
             <entry id="2">second</entry>
           </feed>"#,
    )
    .unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r##"
            import document, { feed } from "./feed.xml";
            if (feed !== document.feed) throw new Error("named export is not the root");
            if (feed["@count"] !== "2") throw new Error("attribute: " + feed["@count"]);
            if (feed.entry.length !== 2) throw new Error("entries: " + feed.entry.length);
            if (feed.entry[1]["#text"] !== "second") {
              throw new Error("text: " + JSON.stringify(feed.entry[1]));
            }
        "##,
    )
    .unwrap();

    let mut runtime = Runtime::builder().build().unwrap();
    runtime.run_module(&main).unwrap();
}

#[test]
fn an_xml_file_that_is_not_well_formed_fails_to_load() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("broken.xml"), "<a><b></a>").unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(&main, "import \"./broken.xml\";\n").unwrap();

    let mut runtime = Runtime::builder().build().unwrap();
    let error = format!("{:?}", runtime.run_module(&main).unwrap_err());
    assert!(error.contains("</b>"), "unexpected error: {error}");
}

#[test]
fn an_import_attribute_names_the_format_whatever_the_file_is_called() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("feed.data"), "<feed n='1'><a>x</a></feed>").unwrap();
    std::fs::write(dir.path().join("conf.data"), "{\"port\": 8080}").unwrap();
    std::fs::write(dir.path().join("notes"), "line one\n").unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r##"
            import feed from "./feed.data" with { type: "xml" };
            import conf from "./conf.data" with { type: "json" };
            import notes from "./notes" with { type: "text" };
            if (feed.feed["@n"] !== "1") throw new Error("xml: " + JSON.stringify(feed));
            if (feed.feed.a !== "x") throw new Error("xml child: " + JSON.stringify(feed));
            if (conf.port !== 8080) throw new Error("json: " + JSON.stringify(conf));
            if (notes !== "line one\n") throw new Error("text: " + JSON.stringify(notes));
        "##,
    )
    .unwrap();

    let mut runtime = Runtime::builder().build().unwrap();
    runtime.run_module(&main).unwrap();
}

#[test]
fn one_file_read_as_two_types_in_one_module_gives_two_values() {
    // The `type` attribute is half of what a module asks for, so the same
    // path under two types is two requests, two targets and two bindings —
    // in every import form, including the namespace one.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("both.data"), "<a k='1'>text</a>").unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r##"
            import parsed from "./both.data" with { type: "xml" };
            import raw from "./both.data" with { type: "text" };
            import * as parsedNs from "./both.data" with { type: "xml" };
            import * as rawNs from "./both.data" with { type: "text" };

            if (parsed.a["@k"] !== "1") throw new Error("xml: " + JSON.stringify(parsed));
            if (raw !== "<a k='1'>text</a>") throw new Error("text: " + JSON.stringify(raw));
            if (parsedNs.default !== parsed) throw new Error("xml namespace is not the xml module");
            if (rawNs.default !== raw) throw new Error("text namespace is not the text module");
            if (parsedNs === rawNs) throw new Error("both types share one namespace");
            if (parsedNs.a["@k"] !== "1") throw new Error("xml named export: " + parsedNs.a);
        "##,
    )
    .unwrap();

    let mut runtime = Runtime::builder().build().unwrap();
    runtime.run_module(&main).unwrap();
}

#[test]
fn an_import_attribute_type_nothing_reads_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("thing.data"), "whatever").unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        "import thing from \"./thing.data\" with { type: \"csv\" };\nconsole.log(thing);\n",
    )
    .unwrap();

    let mut runtime = Runtime::builder().build().unwrap();
    let error = format!("{:?}", runtime.run_module(&main).unwrap_err());
    assert!(error.contains("csv"), "unexpected error: {error}");
}
