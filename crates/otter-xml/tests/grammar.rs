//! Grammar coverage for the scanner, one test per production or rule.

use otter_xml::encoding::{Latin1, Utf8, Utf16};
use otter_xml::error::ErrorKind;
use otter_xml::sink::{Piece, Sink};
use otter_xml::tree::{Child, Node, TreeSink, Value, compact};
use otter_xml::{parse_bytes, parse_latin1, parse_utf8, parse_utf16};

fn root(doc: &str) -> Node {
    parse_utf8(doc).unwrap_or_else(|err| panic!("{doc:?}: {err}"))
}

fn kind(doc: &str) -> ErrorKind {
    parse_utf8(doc)
        .map(|node| node.name)
        .expect_err(&format!("{doc:?} should not parse"))
        .kind
}

fn text_children(node: &Node) -> Vec<&str> {
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

#[test]
fn elements_nest_and_keep_document_order() {
    let node = root("<a><b/>t<c>d</c></a>");
    assert_eq!(node.name, "a");
    assert_eq!(node.children.len(), 3);
    assert_eq!(elements(&node).len(), 2);
    assert_eq!(text_children(&node), vec!["t"]);
    assert_eq!(elements(&node)[1].name, "c");
    assert_eq!(text_children(elements(&node)[1]), vec!["d"]);
}

#[test]
fn empty_and_self_closing_elements() {
    assert!(root("<a/>").children.is_empty());
    assert!(root("<a></a>").children.is_empty());
    assert!(root("<a   />").children.is_empty());
    assert_eq!(root("<a></a  >").name, "a");
}

#[test]
fn attributes_keep_order_and_accept_either_quote() {
    let node = root(r#"<a x="1" y='2' z="a'b" w='c"d'/>"#);
    assert_eq!(
        node.attributes,
        vec![
            ("x".to_owned(), "1".to_owned()),
            ("y".to_owned(), "2".to_owned()),
            ("z".to_owned(), "a'b".to_owned()),
            ("w".to_owned(), "c\"d".to_owned()),
        ]
    );
}

#[test]
fn a_greater_than_sign_is_legal_in_an_attribute_value_and_in_text() {
    let node = root("<a b=\"x>y\">p>q</a>");
    assert_eq!(node.attributes[0].1, "x>y");
    assert_eq!(text_children(&node), vec!["p>q"]);
}

#[test]
fn names_may_be_non_ascii_and_prefixed() {
    let node = root("<ns:日本 ns:属性='v'>x</ns:日本>");
    assert_eq!(node.name, "ns:日本");
    assert_eq!(node.attributes[0].0, "ns:属性");
}

#[test]
fn predefined_entities_and_character_references() {
    let node = root("<a>&lt;&gt;&amp;&apos;&quot;&#65;&#x42;&#x1F642;</a>");
    assert_eq!(text_children(&node), vec!["<>&'\"AB\u{1F642}"]);
    let node = root("<a b='&lt;&#10;&#x9;'/>");
    assert_eq!(node.attributes[0].1, "<\n\t");
}

#[test]
fn cdata_is_text_and_hides_markup() {
    let node = root("<a><![CDATA[<b>&amp;]]]></a>");
    assert_eq!(text_children(&node), vec!["<b>&amp;]"]);
    assert!(elements(&node).is_empty());
    // An empty section still closes.
    assert!(root("<a><![CDATA[]]></a>").children.is_empty());
}

#[test]
fn comments_and_processing_instructions_are_dropped() {
    let node = root("<?pi first?><!--head--><a><!--in--><?pi body?>t</a><!--tail--><?pi z?>");
    assert_eq!(node.name, "a");
    assert_eq!(text_children(&node), vec!["t"]);
    assert_eq!(
        kind("<a><!-- -- --></a>"),
        ErrorKind::Expected("no `--` inside a comment")
    );
    // Only the exact name is reserved; a target merely starting with it is not.
    assert_eq!(root("<?xml-stylesheet href='s.xsl'?><a/>").name, "a");
    assert_eq!(
        kind("<?XmL x?><a/>"),
        ErrorKind::Expected("a processing-instruction target other than `xml`")
    );
}

#[test]
fn the_xml_declaration_is_validated() {
    assert_eq!(root("<?xml version=\"1.0\"?><a/>").name, "a");
    assert_eq!(
        root("<?xml version='1.10' encoding='utf-8' standalone='no' ?><a/>").name,
        "a"
    );
    assert!(matches!(
        kind("<?xml version='2.0'?><a/>"),
        ErrorKind::BadDeclaration(_)
    ));
    assert!(matches!(
        kind("<?xml version='1.0' standalone='maybe'?><a/>"),
        ErrorKind::BadDeclaration(_)
    ));
    // Without the whitespace this is not a declaration at all, and `xml` is
    // not a target a processing instruction may have.
    assert_eq!(
        kind("<?xml?><a/>"),
        ErrorKind::Expected("a processing-instruction target other than `xml`")
    );
    // A declaration must open the document; later it is that same reserved
    // target.
    assert_eq!(
        kind(" <?xml version='1.0'?><a/>"),
        ErrorKind::Expected("a processing-instruction target other than `xml`")
    );
}

#[test]
fn a_document_type_declaration_is_stepped_over() {
    assert_eq!(root("<!DOCTYPE a><a/>").name, "a");
    assert_eq!(root("<!DOCTYPE a SYSTEM \"x>y.dtd\"><a/>").name, "a");
    assert_eq!(
        root("<!DOCTYPE a [ <!ELEMENT a (#PCDATA)> <!-- > --> ]><a/>").name,
        "a"
    );
}

#[test]
fn line_ends_are_normalized_everywhere() {
    assert_eq!(
        text_children(&root("<a>x\r\ny\rz\n</a>")),
        vec!["x\ny\nz\n"]
    );
    let node = root("<a><![CDATA[x\r\ny\r]]></a>");
    assert_eq!(text_children(&node), vec!["x\ny\n"]);
}

#[test]
fn attribute_values_get_the_cdata_normalization() {
    // Literal whitespace becomes a space; a reference keeps its character.
    let node = root("<a b='x\ty\nz\r\nw' c='&#9;'/>");
    assert_eq!(node.attributes[0].1, "x y z w");
    assert_eq!(node.attributes[1].1, "\t");
}

#[test]
fn well_formedness_rules_are_enforced() {
    assert_eq!(
        kind("<a><b></a>"),
        ErrorKind::MismatchedEndTag {
            expected: "b".to_owned(),
            found: "a".to_owned(),
        }
    );
    assert_eq!(kind("<a>"), ErrorKind::UnclosedElement("a".to_owned()));
    assert_eq!(kind("<a/><b/>"), ErrorKind::RootElementCount);
    assert_eq!(kind("</a>"), ErrorKind::RootElementCount);
    assert_eq!(kind(""), ErrorKind::RootElementCount);
    assert_eq!(
        kind("<a x='1' x='2'/>"),
        ErrorKind::DuplicateAttribute("x".to_owned())
    );
    assert_eq!(
        kind("<a>]]></a>"),
        ErrorKind::Expected("no literal `]]>` in character data")
    );
    assert_eq!(
        kind("<a b='<'/>"),
        ErrorKind::Expected("no `<` in an attribute value")
    );
    assert_eq!(
        kind("<a>&nope;</a>"),
        ErrorKind::UnknownEntity("nope".to_owned())
    );
    assert_eq!(kind("<a>&#xD800;</a>"), ErrorKind::BadCharacterReference);
    assert_eq!(kind("<a>&#0;</a>"), ErrorKind::BadCharacterReference);
    assert_eq!(kind("<a>&#x110000;</a>"), ErrorKind::BadCharacterReference);
    assert_eq!(kind("<a>\u{1}</a>"), ErrorKind::IllegalCharacter(1));
    assert_eq!(
        kind("<a b=1/>"),
        ErrorKind::Expected("a quoted attribute value")
    );
    assert_eq!(kind("<a b/>"), ErrorKind::Expected("`=`"));
    assert_eq!(kind("<1/>"), ErrorKind::ExpectedName);
    assert_eq!(kind("<a x='1'y='2'/>"), ErrorKind::ExpectedWhitespace);
}

#[test]
fn text_may_not_appear_outside_the_root() {
    assert_eq!(kind("junk<a/>"), ErrorKind::RootElementCount);
    assert_eq!(kind("<a/>junk"), ErrorKind::RootElementCount);
    // Whitespace around it is fine.
    assert_eq!(root(" \n<a/>\t ").name, "a");
}

#[test]
fn nesting_is_capped_rather_than_overflowing_the_stack() {
    let depth = otter_xml::scan::MAX_DEPTH + 1;
    let doc = format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth));
    assert_eq!(kind(&doc), ErrorKind::DepthLimit);
    // Just under the cap parses, iteratively.
    let depth = otter_xml::scan::MAX_DEPTH - 1;
    let doc = format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth));
    assert_eq!(root(&doc).name, "a");
}

#[test]
fn every_encoding_reaches_the_same_tree() {
    let expected = root("<a b='é'>漢</a>");
    let utf8 =
        parse_bytes("<?xml version='1.0' encoding='UTF-8'?><a b='é'>漢</a>".as_bytes()).unwrap();
    assert_eq!(utf8, expected);
    let bom = parse_bytes(b"\xEF\xBB\xBF<a b='\xC3\xA9'>\xE6\xBC\xA2</a>").unwrap();
    assert_eq!(bom, expected);
    let units: Vec<u16> = "<a b='é'>漢</a>".encode_utf16().collect();
    assert_eq!(parse_utf16(&units), Ok(expected.clone()));
    let mut be = vec![0xFEu8, 0xFF];
    for unit in &units {
        be.extend_from_slice(&unit.to_be_bytes());
    }
    assert_eq!(parse_bytes(&be).unwrap(), expected);
    let mut le = vec![0xFFu8, 0xFE];
    for unit in &units {
        le.extend_from_slice(&unit.to_le_bytes());
    }
    assert_eq!(parse_bytes(&le).unwrap(), expected);
    // Latin-1 spells the same two characters differently.
    let latin1 = parse_latin1(b"<a b='\xE9'>x</a>").unwrap();
    assert_eq!(latin1.attributes[0].1, "é");
    let declared =
        parse_bytes(b"<?xml version='1.0' encoding='ISO-8859-1'?><a b='\xE9'>x</a>").unwrap();
    assert_eq!(declared.attributes[0].1, "é");
}

#[test]
fn a_reference_wider_than_the_encoding_widens_the_run() {
    // ISO-8859-1 cannot spell U+1F642, but a reference to it is still legal.
    let node = parse_latin1(b"<a b='&#x1F642;'>x\xE9&#x4E2D;</a>").unwrap();
    assert_eq!(node.attributes[0].1, "\u{1F642}");
    assert_eq!(text_children(&node), vec!["xé中"]);
}

/// Records how each run of text reached the sink.
#[derive(Default)]
struct Provenance {
    text: Vec<(String, bool)>,
}

impl Sink<u8> for Provenance {
    fn start_element(&mut self, _name: Piece<'_, u8>) {}

    fn attribute(&mut self, _name: Piece<'_, u8>, value: Piece<'_, u8>) {
        self.record(value);
    }

    fn text(&mut self, text: Piece<'_, u8>) {
        self.record(text);
    }

    fn end_element(&mut self) {}
}

impl Provenance {
    fn record(&mut self, piece: Piece<'_, u8>) {
        let borrowed = matches!(piece, Piece::Source { .. });
        let text: String = piece.chars::<Utf8>().filter_map(char::from_u32).collect();
        self.text.push((text, borrowed));
    }
}

#[test]
fn untouched_runs_arrive_as_slices_of_the_document() {
    let mut sink = Provenance::default();
    otter_xml::scan::parse::<Utf8, _>(
        b"<a plain='v' ref='&amp;' ws='x y'><![CDATA[c]]>clean&amp;dirty</a>",
        &mut sink,
    )
    .unwrap();
    assert_eq!(
        sink.text,
        vec![
            ("v".to_owned(), true),
            ("&".to_owned(), false),
            ("x y".to_owned(), true),
            ("c".to_owned(), true),
            ("clean&dirty".to_owned(), false),
        ]
    );
}

#[test]
fn the_compact_form_folds_repeats_into_arrays() {
    let node = root(
        "<order id='A1'><customer>Ada</customer><item sku='tea'>Green tea</item>\
         <item sku='mug'>Mug</item><paid/></order>",
    );
    let Value::Object(top) = compact(&node) else {
        panic!("the compact form has one key");
    };
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].0, "order");
    let Value::Object(order) = &top[0].1 else {
        panic!("an element with attributes is an object");
    };
    assert_eq!(order[0], ("@id".to_owned(), Value::Text("A1".to_owned())));
    assert_eq!(
        order[1],
        ("customer".to_owned(), Value::Text("Ada".to_owned()))
    );
    let Value::Array(items) = &order[2].1 else {
        panic!("a repeated name becomes an array");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(order[3], ("paid".to_owned(), Value::Text(String::new())));
}

#[test]
fn the_compact_form_keeps_text_beside_other_content() {
    let node = root("<p class='lead'>Hello <b>world</b>!</p>");
    let Value::Object(top) = compact(&node) else {
        panic!("one key");
    };
    let Value::Object(p) = &top[0].1 else {
        panic!("object");
    };
    assert_eq!(p[0].0, "@class");
    assert_eq!(p[1].0, "b");
    assert_eq!(
        p[2],
        ("#text".to_owned(), Value::Text("Hello !".to_owned()))
    );
}

#[test]
fn a_sink_may_be_driven_directly_over_utf16() {
    let units: Vec<u16> = "<a>漢</a>".encode_utf16().collect();
    let mut sink = TreeSink::<Utf16>::new();
    otter_xml::scan::parse::<Utf16, _>(&units, &mut sink).unwrap();
    assert_eq!(text_children(&sink.finish().unwrap()), vec!["漢"]);
    let mut sink = TreeSink::<Latin1>::new();
    otter_xml::scan::parse::<Latin1, _>(b"<a>\xFF</a>", &mut sink).unwrap();
    assert_eq!(text_children(&sink.finish().unwrap()), vec!["ÿ"]);
}
