//! The internal document type subset: what its declarations change about a
//! document, and the limits that keep expansion finite.

use otter_xml::error::ErrorKind;
use otter_xml::parse_utf8;
use otter_xml::tree::{Child, Node};

fn root(doc: &str) -> Node {
    parse_utf8(doc).unwrap_or_else(|err| panic!("{doc:?}: {err}"))
}

fn kind(doc: &str) -> ErrorKind {
    parse_utf8(doc)
        .map(|node| node.name)
        .expect_err(&format!("{doc:?} should not parse"))
        .kind
}

fn text(node: &Node) -> String {
    node.children
        .iter()
        .filter_map(|child| match child {
            Child::Text(text) => Some(text.as_str()),
            Child::Element(_) => None,
        })
        .collect()
}

fn elements(node: &Node) -> Vec<&Node> {
    node.children
        .iter()
        .filter_map(|child| match child {
            Child::Element(element) => Some(element),
            Child::Text(_) => None,
        })
        .collect()
}

fn attribute<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.attributes
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

#[test]
fn an_internal_entity_is_expanded_where_it_is_used() {
    let node = root(r#"<!DOCTYPE a [<!ENTITY who "world">]><a>hello &who;!</a>"#);
    assert_eq!(text(&node), "hello world!");

    // References inside replacement text are expanded in turn, and character
    // references in the declaration are resolved as it is read.
    let node = root(
        r#"<!DOCTYPE a [
             <!ENTITY inner "deep">
             <!ENTITY outer "&inner;&#x21;">
           ]><a>&outer;</a>"#,
    );
    assert_eq!(text(&node), "deep!");
}

#[test]
fn replacement_text_that_holds_markup_is_parsed_as_content() {
    let node = root(r#"<!DOCTYPE a [<!ENTITY row "<b x='1'>t</b>">]><a>&row;&row;</a>"#);
    let children = elements(&node);
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].name, "b");
    assert_eq!(attribute(children[0], "x"), Some("1"));
    assert_eq!(text(children[0]), "t");

    // An element opened inside an entity has to close inside it.
    assert!(matches!(
        kind(r#"<!DOCTYPE a [<!ENTITY half "<b>">]><a>&half;</a></b>"#),
        ErrorKind::UnclosedElement(name) if name == "b"
    ));
    assert!(matches!(
        kind(r#"<!DOCTYPE a [<!ENTITY tail "</b>">]><a><b>&tail;</b></a>"#),
        ErrorKind::UnexpectedEndTag(name) if name == "b"
    ));
}

#[test]
fn an_entity_may_be_used_in_an_attribute_value() {
    let node = root(r#"<!DOCTYPE a [<!ENTITY v "one two">]><a k="&v;"/>"#);
    assert_eq!(attribute(&node, "k"), Some("one two"));

    // White space in replacement text normalizes like white space written in
    // the value itself.
    let node = root("<!DOCTYPE a [<!ENTITY v \"one\ttwo\">]><a k=\"&v;\"/>");
    assert_eq!(attribute(&node, "k"), Some("one two"));

    // Markup may not reach an attribute value, however it is spelled.
    assert!(matches!(
        kind(r#"<!DOCTYPE a [<!ENTITY v "<b/>">]><a k="&v;"/>"#),
        ErrorKind::Expected(_)
    ));
}

#[test]
fn an_entity_that_refers_to_itself_is_refused() {
    assert!(matches!(
        kind(r#"<!DOCTYPE a [<!ENTITY loop "&loop;">]><a>&loop;</a>"#),
        ErrorKind::RecursiveEntity(name) if name == "loop"
    ));
    assert!(matches!(
        kind(r#"<!DOCTYPE a [<!ENTITY one "&two;"><!ENTITY two "&one;">]><a>&one;</a>"#),
        ErrorKind::RecursiveEntity(_)
    ));
}

#[test]
fn exponential_expansion_fails_instead_of_exhausting_memory() {
    let mut doc = String::from("<!DOCTYPE a [<!ENTITY e0 \"");
    doc.push_str(&"x".repeat(64));
    doc.push_str("\">");
    for level in 1..=20 {
        doc.push_str(&format!(
            "<!ENTITY e{level} \"&e{};&e{};&e{};&e{};&e{};&e{};&e{};&e{};&e{};&e{};\">",
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
            level - 1,
        ));
    }
    doc.push_str("]><a>&e20;</a>");
    assert_eq!(kind(&doc), ErrorKind::EntityExpansionLimit);
}

#[test]
fn attribute_declarations_supply_the_values_a_tag_leaves_out() {
    let node = root(
        r#"<!DOCTYPE a [
             <!ATTLIST a k CDATA "fallback" m CDATA #IMPLIED n CDATA #FIXED "pinned">
           ]><a/>"#,
    );
    assert_eq!(attribute(&node, "k"), Some("fallback"));
    assert_eq!(attribute(&node, "n"), Some("pinned"));
    assert_eq!(attribute(&node, "m"), None);

    // What the tag wrote wins, and keeps its place before what is supplied.
    let node =
        root(r#"<!DOCTYPE a [<!ATTLIST a k CDATA "fallback" z CDATA "last">]><a k="written"/>"#);
    assert_eq!(node.attributes.len(), 2);
    assert_eq!(node.attributes[0], ("k".to_owned(), "written".to_owned()));
    assert_eq!(node.attributes[1], ("z".to_owned(), "last".to_owned()));

    // A default value is normalized as the same value on a tag would be.
    let node = root(r#"<!DOCTYPE a [<!ENTITY v "x"><!ATTLIST a k CDATA "&v; &#65;">]><a/>"#);
    assert_eq!(attribute(&node, "k"), Some("x A"));
}

#[test]
fn a_declared_type_other_than_cdata_collapses_spaces() {
    let node = root("<!DOCTYPE a [<!ATTLIST a k NMTOKENS #IMPLIED>]><a k=\"  one \t two  \"/>");
    assert_eq!(attribute(&node, "k"), Some("one two"));

    // CDATA keeps every space the normalization produced.
    let node = root("<!DOCTYPE a [<!ATTLIST a k CDATA #IMPLIED>]><a k=\"  one \t two  \"/>");
    assert_eq!(attribute(&node, "k"), Some("  one   two  "));

    // Enumerations and notations are tokenized types too.
    let node = root("<!DOCTYPE a [<!ATTLIST a k (one|two) \"one\">]><a k=\" two \"/>");
    assert_eq!(attribute(&node, "k"), Some("two"));
}

#[test]
fn a_parameter_entity_carries_declarations() {
    let node = root(
        r#"<!DOCTYPE a [
             <!ENTITY % common "<!ENTITY who 'world'><!ATTLIST a k CDATA 'default'>">
             %common;
           ]><a>&who;</a>"#,
    );
    assert_eq!(text(&node), "world");
    assert_eq!(attribute(&node, "k"), Some("default"));

    // A parameter-entity reference may not appear inside a declaration.
    assert!(matches!(
        kind(r#"<!DOCTYPE a [<!ENTITY % p "x"><!ENTITY e "%p;">]><a>&e;</a>"#),
        ErrorKind::BadDoctype(_)
    ));
}

#[test]
fn element_and_notation_declarations_are_read_and_dropped() {
    let node = root(
        r#"<!DOCTYPE a [
             <!ELEMENT a (b)*>
             <!ELEMENT b (#PCDATA)>
             <!NOTATION png SYSTEM "image/png">
             <!ENTITY logo SYSTEM "logo.png" NDATA png>
           ]><a><b>t</b></a>"#,
    );
    assert_eq!(elements(&node).len(), 1);

    // Unparsed data has no text to stand in for a reference.
    assert!(matches!(
        kind(
            r#"<!DOCTYPE a [<!NOTATION png SYSTEM "i"><!ENTITY logo SYSTEM "l" NDATA png>]><a>&logo;</a>"#
        ),
        ErrorKind::UnparsedEntityReference(name) if name == "logo"
    ));
}

#[test]
fn an_undeclared_entity_depends_on_what_the_document_promised() {
    // Nothing outside to appeal to: the reference is an error.
    assert!(matches!(
        kind("<a>&missing;</a>"),
        ErrorKind::UnknownEntity(name) if name == "missing"
    ));
    assert!(matches!(
        kind(r#"<!DOCTYPE a [<!ENTITY here "x">]><a>&missing;</a>"#),
        ErrorKind::UnknownEntity(_)
    ));

    // A subset this parser does not read may hold the declaration, so the
    // reference is passed over rather than rejected.
    let node = root(r#"<!DOCTYPE a SYSTEM "outside.dtd"><a>x&missing;y</a>"#);
    assert_eq!(text(&node), "xy");

    // Unless the document said it stands alone.
    assert!(matches!(
        kind(
            r#"<?xml version="1.0" standalone="yes"?><!DOCTYPE a SYSTEM "o.dtd"><a>&missing;</a>"#
        ),
        ErrorKind::UnknownEntity(_)
    ));
}

#[test]
fn a_malformed_subset_is_reported_rather_than_skipped() {
    assert!(matches!(
        kind("<!DOCTYPE a [<!WHAT>]><a/>"),
        ErrorKind::BadDoctype(_)
    ));
    assert!(matches!(
        kind(r#"<!DOCTYPE a [<!ENTITY e "unterminated]><a/>"#),
        ErrorKind::UnexpectedEof
    ));
    assert!(matches!(
        kind("<!DOCTYPE a [<!ATTLIST a k WHAT #IMPLIED>]><a/>"),
        ErrorKind::BadDoctype(_)
    ));
    // A `>` inside a literal does not end the declaration it sits in.
    assert_eq!(
        root(r#"<!DOCTYPE a [<!ENTITY e "a > b">]><a>&e;</a>"#).name,
        "a"
    );
}
