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
