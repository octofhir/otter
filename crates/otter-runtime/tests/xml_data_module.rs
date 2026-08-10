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
