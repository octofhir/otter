//! Run the W3C XML Conformance Test Suite against this parser.
//!
//! The suite is not vendored: download `xmlts20130923.zip` from
//! <https://www.w3.org/XML/Test/>, unpack it, and point this at the
//! `xmlconf` directory it contains:
//!
//! ```text
//! cargo run -p otter-xml --example w3c -- <path>/xmlconf
//! ```
//!
//! Only the cases whose outcome is defined for what this parser is are run: a
//! non-validating XML 1.0 (Fifth Edition) processor that reads no external
//! entity. That means the cases needing an external entity are skipped, as are
//! the XML 1.1, namespace and `error`-class cases, and an `invalid` case is
//! expected to parse, since validity is not this parser's business.

use std::path::{Path, PathBuf};

use otter_xml::tree::{Child, Node};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next() else {
        eprintln!("usage: w3c <path to the suite's xmlconf directory>");
        std::process::exit(2);
    };
    let root = PathBuf::from(root);
    let mut report = Report::default();
    for catalog in catalogs(&root) {
        run_catalog(&root, &catalog, &mut report);
    }
    report.print();
    if report.failed > 0 {
        std::process::exit(1);
    }
}

/// What the run found.
#[derive(Default)]
struct Report {
    run: usize,
    failed: usize,
    failures: Vec<String>,
    /// Why cases were left out, so the skipped count says something.
    skipped: Vec<(&'static str, usize)>,
}

impl Report {
    fn skip(&mut self, reason: &'static str) {
        match self.skipped.iter_mut().find(|(known, _)| *known == reason) {
            Some((_, count)) => *count += 1,
            None => self.skipped.push((reason, 1)),
        }
    }

    fn print(&self) {
        for failure in &self.failures {
            println!("FAIL {failure}");
        }
        let skipped: usize = self.skipped.iter().map(|(_, count)| count).sum();
        println!(
            "ran {} — passed {} — failed {} — skipped {skipped}",
            self.run,
            self.run - self.failed,
            self.failed,
        );
        for (reason, count) in &self.skipped {
            println!("  skipped {count} — {reason}");
        }
    }
}

/// The per-vendor catalogs, which the top-level catalog names as external
/// entities and this parser deliberately does not read for itself.
fn catalogs(root: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(root.join("xmlconf.xml")).expect("xmlconf.xml");
    let subset = text
        .split_once('[')
        .and_then(|(_, rest)| rest.split_once(']'))
        .map_or("", |(subset, _)| subset);
    let mut out = Vec::new();
    for declaration in subset.split("<!ENTITY").skip(1) {
        let Some((_, rest)) = declaration.split_once("SYSTEM") else {
            continue;
        };
        let Some((_, rest)) = rest.split_once('"') else {
            continue;
        };
        let Some((path, _)) = rest.split_once('"') else {
            continue;
        };
        out.push(path.to_owned());
    }
    out
}

fn run_catalog(root: &Path, catalog: &str, report: &mut Report) {
    let path = root.join(catalog);
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("missing catalog {}", path.display());
        return;
    };
    // A catalog is an external entity of the suite's own document, so some of
    // them are fragments with no single root; wrapping gives every one a root
    // without changing what it says.
    let mut wrapped = b"<CATALOG>".to_vec();
    wrapped.extend_from_slice(strip_declaration(&bytes));
    wrapped.extend_from_slice(b"</CATALOG>");
    let tree = match otter_xml::parse_bytes(&wrapped) {
        Ok(tree) => tree,
        Err(error) => {
            eprintln!("catalog {catalog} does not parse: {error}");
            return;
        }
    };
    let base = path.parent().unwrap_or(root).to_path_buf();
    walk(&tree, &base, report);
}

/// Everything after an XML declaration, which a wrapped fragment must not
/// carry into the middle of a document.
fn strip_declaration(bytes: &[u8]) -> &[u8] {
    if !bytes.starts_with(b"<?xml") {
        return bytes;
    }
    match bytes.windows(2).position(|pair| pair == b"?>") {
        Some(end) => &bytes[end + 2..],
        None => bytes,
    }
}

fn attribute<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.attributes
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn walk(node: &Node, base: &Path, report: &mut Report) {
    let base = match attribute(node, "xml:base") {
        Some(relative) => base.join(relative),
        None => base.to_path_buf(),
    };
    if node.name == "TEST" {
        run_test(node, &base, report);
    }
    for child in &node.children {
        if let Child::Element(element) = child {
            walk(element, &base, report);
        }
    }
}

fn run_test(node: &Node, base: &Path, report: &mut Report) {
    let id = attribute(node, "ID").unwrap_or("?");
    let kind = attribute(node, "TYPE").unwrap_or("");
    let entities = attribute(node, "ENTITIES").unwrap_or("none");
    let recommendation = attribute(node, "RECOMMENDATION").unwrap_or("XML1.0");
    let version = attribute(node, "VERSION").unwrap_or("1.0");
    let editions = attribute(node, "EDITION");

    let left_out = if entities != "none" {
        Some("needs an external entity, which this parser does not read")
    } else if version == "1.1" {
        Some("XML 1.1")
    } else if !recommendation.starts_with("XML1.0") {
        Some("not an XML 1.0 recommendation")
    } else if editions.is_some_and(|list| !list.split_whitespace().any(|edition| edition == "5")) {
        Some("not an XML 1.0 Fifth Edition case")
    } else if kind == "error" {
        Some("`error` class, whose outcome the suite leaves optional")
    } else if !matches!(kind, "valid" | "invalid" | "not-wf") {
        Some("no test type")
    } else {
        None
    };
    if let Some(reason) = left_out {
        report.skip(reason);
        return;
    }
    let Some(uri) = attribute(node, "URI") else {
        report.skip("no document to read");
        return;
    };
    let path = base.join(uri);
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("missing case {}", path.display());
        report.skip("the suite has no such file");
        return;
    };
    report.run += 1;
    let outcome = otter_xml::parse_bytes(&bytes);
    let ok = match kind {
        "not-wf" => outcome.is_err(),
        _ => outcome.is_ok(),
    };
    if !ok {
        report.failed += 1;
        let detail = match outcome {
            Ok(_) => "parsed, but is not well-formed".to_owned(),
            Err(error) => error.to_string(),
        };
        report
            .failures
            .push(format!("{id} [{kind}] {}: {detail}", path.display()));
    }
}
