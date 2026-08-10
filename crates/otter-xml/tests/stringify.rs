//! Writing documents back out: escaping, refusal, indentation, and the
//! round trip against the parser.

use otter_xml::error::ErrorKind;
use otter_xml::stringify;
use otter_xml::tree::{Child, Node, Value, compact};
use otter_xml::{parse_bytes, parse_utf8};

fn written(doc: &str) -> String {
    let node = parse_utf8(doc).unwrap_or_else(|err| panic!("{doc:?}: {err}"));
    stringify::node(&node, None).unwrap_or_else(|err| panic!("{doc:?}: {err}"))
}

fn written_compact(doc: &str) -> String {
    let node = parse_utf8(doc).unwrap_or_else(|err| panic!("{doc:?}: {err}"));
    stringify::value(&compact(&node), None).unwrap_or_else(|err| panic!("{doc:?}: {err}"))
}

#[test]
fn a_document_survives_being_written_and_read_again() {
    let documents = [
        "<a/>",
        "<a></a>",
        "<a b='1' c='two'/>",
        "<a><b/><b/><c/></a>",
        "<a>text</a>",
        "<a>before<b/>after</a>",
        "<a b='  spaced  value '>x</a>",
        "<a>&lt;&amp;&gt;</a>",
        "<a b='&quot;quoted&quot;'/>",
        "<a><b><c><d>deep</d></c></b></a>",
        "<a xml:lang='en' x:y='z'/>",
        "<a>\u{1F600} \u{4E2D}\u{6587}</a>",
    ];
    for doc in documents {
        let first = parse_utf8(doc).unwrap_or_else(|err| panic!("{doc:?}: {err}"));
        let text = stringify::node(&first, None).unwrap();
        let again = parse_utf8(&text).unwrap_or_else(|err| panic!("{text:?}: {err}"));
        assert_eq!(first, again, "{doc:?} became {text:?}");

        let compacted = compact(&first);
        let text = stringify::value(&compacted, None).unwrap();
        let again = parse_utf8(&text).unwrap_or_else(|err| panic!("{text:?}: {err}"));
        assert_eq!(compacted, compact(&again), "{doc:?} became {text:?}");
    }
}

#[test]
fn what_a_document_may_not_say_verbatim_is_escaped() {
    assert_eq!(
        written("<a>1 &lt; 2 &amp;&amp; 3 &gt; 2</a>"),
        "<a>1 &lt; 2 &amp;&amp; 3 &gt; 2</a>"
    );
    // A tab or newline in an attribute value would be read back as a space,
    // so it is written as a reference.
    let node = Node {
        name: "a".to_owned(),
        attributes: vec![("k".to_owned(), "one\ttwo\nthree\rfour".to_owned())],
        children: Vec::new(),
    };
    let text = stringify::node(&node, None).unwrap();
    assert_eq!(text, "<a k=\"one&#9;two&#10;three&#13;four\"/>");
    assert_eq!(parse_utf8(&text).unwrap(), node);

    // The same for a carriage return in character data, which line-end
    // normalization would otherwise turn into a newline.
    let node = Node {
        name: "a".to_owned(),
        attributes: Vec::new(),
        children: vec![Child::Text("one\rtwo".to_owned())],
    };
    let text = stringify::node(&node, None).unwrap();
    assert_eq!(text, "<a>one&#13;two</a>");
    assert_eq!(parse_utf8(&text).unwrap(), node);
}

#[test]
fn an_empty_element_keeps_its_short_form() {
    assert_eq!(written("<a></a>"), "<a/>");
    assert_eq!(written("<a>   </a>"), "<a>   </a>");
    assert_eq!(written_compact("<a></a>"), "<a/>");
    assert_eq!(written_compact("<a k='v'></a>"), "<a k=\"v\"/>");
}

#[test]
fn a_name_that_xml_cannot_spell_is_refused() {
    let illegal = |name: &str| Node {
        name: name.to_owned(),
        ..Node::default()
    };
    for name in ["", "1a", "a b", "a<b", "-a", "a\u{0}"] {
        assert!(
            matches!(
                stringify::node(&illegal(name), None).unwrap_err().kind,
                ErrorKind::IllegalName(_)
            ),
            "{name:?} should not be written"
        );
    }
    assert!(stringify::node(&illegal("a-b.c:d"), None).is_ok());

    let attribute = Node {
        name: "a".to_owned(),
        attributes: vec![("not a name".to_owned(), "v".to_owned())],
        children: Vec::new(),
    };
    assert!(matches!(
        stringify::node(&attribute, None).unwrap_err().kind,
        ErrorKind::IllegalName(_)
    ));
}

#[test]
fn a_value_that_is_not_a_document_is_refused() {
    let refused = |value: Value| {
        matches!(
            stringify::value(&value, None).unwrap_err().kind,
            ErrorKind::Unserializable(_)
        )
    };
    assert!(refused(Value::Text("bare".to_owned())));
    assert!(refused(Value::Array(vec![Value::Text("x".to_owned())])));
    assert!(refused(Value::Object(Vec::new())));
    assert!(refused(Value::Object(vec![
        ("a".to_owned(), Value::Text(String::new())),
        ("b".to_owned(), Value::Text(String::new())),
    ])));
    assert!(refused(Value::Object(vec![(
        "@a".to_owned(),
        Value::Text(String::new())
    )])));
    // An attribute holds text, never an element.
    assert!(refused(Value::Object(vec![(
        "a".to_owned(),
        Value::Object(vec![("@k".to_owned(), Value::Object(Vec::new()))]),
    )])));
}

#[test]
fn indenting_lays_out_element_only_content() {
    let node = parse_utf8("<a><b><c x='1'/></b><b>text</b><d/></a>").unwrap();
    let text = stringify::node(&node, Some("  ")).unwrap();
    assert_eq!(
        text,
        "<a>\n  <b>\n    <c x=\"1\"/>\n  </b>\n  <b>text</b>\n  <d/>\n</a>"
    );
    // Indented output is still a document, and its elements are the same
    // ones; only the white space between them is new.
    let again = parse_utf8(&text).unwrap();
    assert_eq!(stringify::node(&again, Some("  ")).unwrap(), text);

    let compacted = compact(&parse_utf8("<a><b k='1'/><b k='2'/><c>t</c></a>").unwrap());
    assert_eq!(
        stringify::value(&compacted, Some("\t")).unwrap(),
        "<a>\n\t<b k=\"1\"/>\n\t<b k=\"2\"/>\n\t<c>t</c>\n</a>"
    );
    // No indent, no white space of its own.
    assert_eq!(
        stringify::value(&compacted, None).unwrap(),
        "<a><b k=\"1\"/><b k=\"2\"/><c>t</c></a>"
    );
}

#[test]
fn text_written_by_a_declaration_reads_back_the_same() {
    // Entity expansion and attribute defaults are part of what `parse`
    // produced, so writing that out has to reproduce them literally.
    let doc = r#"<!DOCTYPE a [
        <!ENTITY who "world &#38;amp; friends">
        <!ATTLIST a k CDATA "fallback">
      ]><a>hello &who;</a>"#;
    let node = parse_bytes(doc.as_bytes()).unwrap();
    let text = stringify::node(&node, None).unwrap();
    // The `&#38;` of the declaration became `&` in the replacement text, and
    // the `&amp;` that made is a reference again where the entity was used.
    assert_eq!(text, "<a k=\"fallback\">hello world &amp; friends</a>");
    assert_eq!(parse_utf8(&text).unwrap(), node);
}
