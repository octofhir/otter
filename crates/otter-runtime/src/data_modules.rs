//! Data files loaded as modules.
//!
//! Configuration and fixtures arrive as YAML, TOML, JSONC, JSON5, or plain
//! text far more often than as strict JSON, and every project that imports one
//! today either hand-rolls a loader or adds a bundler plugin. Otter loads them
//! directly: the file is parsed by the host and handed to the module graph as
//! a single default export, exactly as a `.json` import already is.
//!
//! # Contents
//! - [`DataFormat`] — the formats recognized by extension.
//! - [`data_module_source`] — turn a data file's text into module source.
//! - [`MAX_DATA_NESTING`] — the nesting bound applied before parsing.
//!
//! # Invariants
//! - Nesting is bounded before a parser sees the text. Every parser here
//!   descends recursively, and so does dropping the value it returns, so a
//!   deeply nested document does not fail — it exhausts the stack and takes
//!   the process with it. The bound is applied to the source bytes because
//!   that is the last point at which the recursion can still be prevented,
//!   and because none of these parsers exposes a depth limit of its own.
//! - A parsed document is re-emitted as JSON, so the module body is a single
//!   literal with no host values crossing into the graph.
//! - Text files load as a string, not as code.
//! - XML is read from bytes rather than text, because its encoding is settled
//!   by a byte-order mark or its own declaration, and it is parsed straight
//!   into the compact shape without a `serde_json::Value` in between.
//!
//! # See also
//! - [`crate::module_loader`] for where these modules enter the graph.

use serde::Serialize;

/// Deepest `{`/`[` nesting accepted in an imported data file.
///
/// Chosen well below the depth a small main-thread stack survives — the
/// tightest budget is around 1 MiB, where recursive descent gives out in the
/// low hundreds of levels — while sitting far above any hand-written or
/// tool-generated configuration.
pub const MAX_DATA_NESTING: usize = 128;

/// A data format Otter can load as a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataFormat {
    /// Strict JSON.
    Json,
    /// JSON with comments and trailing commas.
    Jsonc,
    /// JSON5.
    Json5,
    /// YAML.
    Yaml,
    /// TOML.
    Toml,
    /// XML 1.0, loaded in the compact shape.
    Xml,
    /// Plain text, loaded as a string.
    Text,
}

impl DataFormat {
    /// Recognize a data format from a `with { type: "…" }` attribute.
    ///
    /// The attribute names the format outright, so it decides regardless of
    /// what the file is called — that is the point of writing it.
    #[must_use]
    pub fn from_attribute(name: &str) -> Option<Self> {
        Some(match name {
            "json" => Self::Json,
            "jsonc" => Self::Jsonc,
            "json5" => Self::Json5,
            "yaml" => Self::Yaml,
            "toml" => Self::Toml,
            "xml" => Self::Xml,
            "text" => Self::Text,
            _ => return None,
        })
    }

    /// Recognize a data format from a file extension.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        Some(match extension {
            "json" => Self::Json,
            "jsonc" => Self::Jsonc,
            "json5" => Self::Json5,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "xml" => Self::Xml,
            "txt" | "text" => Self::Text,
            _ => return None,
        })
    }
}

/// Failure to turn a data file into a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataModuleError {
    /// Human-readable reason.
    pub message: String,
}

/// Turn a data file's bytes into module source exporting its parsed value.
///
/// # Errors
/// Returns the parser's complaint, or the nesting bound's, as a message.
pub fn data_module_source(format: DataFormat, bytes: &[u8]) -> Result<String, DataModuleError> {
    let parsed = data_literal(format, bytes)?;
    let literal = parsed.literal;
    let mut source = format!("const document = {literal};\nexport default document;\n");
    // An XML document's root element is worth naming, so
    // `import { feed } from "./x.xml"` works — but only where its name is one
    // JavaScript can bind.
    if let Some(name) = parsed.root.filter(|name| is_identifier(name)) {
        source.push_str(&format!("export const {name} = document[{name:?}];\n"));
    }
    Ok(source)
}

/// Turn a data file's bytes into CommonJS module source publishing its parsed
/// value as `module.exports`.
///
/// # Errors
/// As [`data_module_source`].
pub fn data_module_commonjs_source(
    format: DataFormat,
    bytes: &[u8],
) -> Result<String, DataModuleError> {
    let literal = data_literal(format, bytes)?.literal;
    Ok(format!("module.exports = ({literal});\n"))
}

/// A data file's value as a JavaScript literal, and the root element's name
/// where the format has one.
struct ParsedData {
    literal: String,
    root: Option<String>,
}

/// Parse a data file and write its value as one JavaScript literal, which is
/// what both module forms publish — so the two cannot disagree about it.
fn data_literal(format: DataFormat, bytes: &[u8]) -> Result<ParsedData, DataModuleError> {
    if format == DataFormat::Xml {
        let root = otter_xml::parse_bytes(bytes).map_err(parse_error)?;
        let document = otter_xml::compact(&root);
        let mut literal = String::new();
        write_json(&document, &mut literal);
        return Ok(ParsedData {
            literal,
            root: Some(root.name),
        });
    }
    let text = std::str::from_utf8(bytes).map_err(parse_error)?;
    if format == DataFormat::Text {
        return Ok(ParsedData {
            literal: json_literal(&text),
            root: None,
        });
    }
    if let Some(depth) = nesting_depth_over(text, MAX_DATA_NESTING) {
        return Err(DataModuleError {
            message: format!(
                "data file nests more than {MAX_DATA_NESTING} levels deep (found {depth})"
            ),
        });
    }
    let value: serde_json::Value = match format {
        DataFormat::Json => serde_json::from_str(text).map_err(parse_error)?,
        DataFormat::Jsonc => jsonc_parser::parse_to_serde_value(text, &jsonc_options())
            .map_err(parse_error)?
            .unwrap_or(serde_json::Value::Null),
        DataFormat::Json5 => json5::from_str(text).map_err(parse_error)?,
        DataFormat::Yaml => serde_yaml::from_str(text).map_err(parse_error)?,
        DataFormat::Toml => toml::from_str(text).map_err(parse_error)?,
        DataFormat::Text | DataFormat::Xml => {
            unreachable!("text and XML are handled before parsing")
        }
    };
    Ok(ParsedData {
        literal: json_literal(&value),
        root: None,
    })
}

/// Write a compact-shape value as JSON.
///
/// Written here rather than through `serde_json` because the value is the
/// parser's own and needs no intermediate representation, and because the
/// module body is JavaScript source: the two line separators that JSON allows
/// raw and JavaScript once did not have to be escaped either way.
fn write_json(value: &otter_xml::Value, out: &mut String) {
    match value {
        otter_xml::Value::Text(text) => write_json_string(text, out),
        otter_xml::Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        otter_xml::Value::Object(entries) => {
            out.push('{');
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_json(item, out);
            }
            out.push('}');
        }
    }
}

fn write_json_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Legal in a JSON string, but a line terminator in JavaScript
            // source before ES2019, and cheap to escape regardless.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Whether `name` is a plain JavaScript identifier that is not a keyword.
fn is_identifier(name: &str) -> bool {
    const RESERVED: [&str; 37] = [
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "function",
        "if",
        "import",
        "in",
        "instanceof",
        "let",
        "new",
        "null",
        "return",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "typeof",
        "var",
        "void",
    ];
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        return false;
    }
    !RESERVED.contains(&name)
}

/// Pin JSONC acceptance to one dialect, so a parser upgrade cannot silently
/// widen what an imported file may contain.
fn jsonc_options() -> jsonc_parser::ParseOptions {
    jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
    }
}

fn parse_error(error: impl std::fmt::Display) -> DataModuleError {
    DataModuleError {
        message: error.to_string(),
    }
}

/// A JSON literal is a JavaScript expression, so a parsed document crosses
/// into the module graph as source rather than as a host value.
fn json_literal(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// Depth of the deepest `{`/`[` nesting, when it exceeds `limit`.
///
/// Scanning bytes is safe: every delimiter is ASCII and every UTF-8
/// continuation byte is `>= 0x80`, so no multi-byte character can be mistaken
/// for one. Delimiters inside strings and comments do not count.
fn nesting_depth_over(text: &str, limit: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut index = 0usize;
    let mut in_string: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(quote) = in_string {
            match byte {
                b'\\' => index += 1,
                _ if byte == quote => in_string = None,
                _ => {}
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => in_string = Some(byte),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index += 2;
                continue;
            }
            b'#' => {
                // A comment in YAML and TOML; harmless to skip in the others,
                // where it cannot begin a delimiter either.
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            b'{' | b'[' => {
                depth += 1;
                if depth > limit {
                    return Some(depth);
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_extension_maps_to_a_format() {
        assert_eq!(DataFormat::from_extension("yaml"), Some(DataFormat::Yaml));
        assert_eq!(DataFormat::from_extension("yml"), Some(DataFormat::Yaml));
        assert_eq!(DataFormat::from_extension("toml"), Some(DataFormat::Toml));
        assert_eq!(DataFormat::from_extension("jsonc"), Some(DataFormat::Jsonc));
        assert_eq!(DataFormat::from_extension("json5"), Some(DataFormat::Json5));
        assert_eq!(DataFormat::from_extension("txt"), Some(DataFormat::Text));
        assert_eq!(DataFormat::from_extension("xml"), Some(DataFormat::Xml));
        assert_eq!(DataFormat::from_attribute("xml"), Some(DataFormat::Xml));
        assert_eq!(DataFormat::from_attribute("text"), Some(DataFormat::Text));
        assert_eq!(DataFormat::from_attribute("yml"), None);
        assert_eq!(DataFormat::from_attribute("javascript"), None);
        assert_eq!(DataFormat::from_extension("ts"), None);
    }

    #[test]
    fn yaml_and_toml_load_as_one_default_export() {
        assert_eq!(
            data_module_source(
                DataFormat::Yaml,
                "port: 8080\nhosts:\n  - a\n  - b\n".as_bytes()
            )
            .unwrap(),
            "const document = {\"port\":8080,\"hosts\":[\"a\",\"b\"]};\nexport default document;\n"
        );
        assert_eq!(
            data_module_source(
                DataFormat::Toml,
                "port = 8080\nhosts = [\"a\"]\n".as_bytes()
            )
            .unwrap(),
            "const document = {\"hosts\":[\"a\"],\"port\":8080};\nexport default document;\n"
        );
    }

    #[test]
    fn jsonc_and_json5_accept_what_json_rejects() {
        assert_eq!(
            data_module_source(
                DataFormat::Jsonc,
                "{\n // a comment\n \"a\": 1,\n}".as_bytes()
            )
            .unwrap(),
            "const document = {\"a\":1};\nexport default document;\n"
        );
        assert_eq!(
            data_module_source(DataFormat::Json5, "{ a: 1, b: 'two' }".as_bytes()).unwrap(),
            "const document = {\"a\":1,\"b\":\"two\"};\nexport default document;\n"
        );
    }

    #[test]
    fn text_loads_as_a_string() {
        assert_eq!(
            data_module_source(DataFormat::Text, "line one\nline two\n".as_bytes()).unwrap(),
            "const document = \"line one\\nline two\\n\";\nexport default document;\n"
        );
    }

    #[test]
    fn xml_loads_in_the_compact_shape_and_names_its_root() {
        let source = data_module_source(
            DataFormat::Xml,
            b"<feed count='2'><entry>one</entry><entry>two</entry></feed>",
        )
        .unwrap();
        assert_eq!(
            source,
            concat!(
                "const document = {\"feed\":{\"@count\":\"2\",",
                "\"entry\":[\"one\",\"two\"]}};\n",
                "export default document;\n",
                "export const feed = document[\"feed\"];\n",
            )
        );
    }

    #[test]
    fn require_publishes_the_same_value_as_import() {
        assert_eq!(
            data_module_commonjs_source(DataFormat::Json, b"{\"a\": [1, 2]}").unwrap(),
            "module.exports = ({\"a\":[1,2]});\n"
        );
        assert_eq!(
            data_module_commonjs_source(DataFormat::Xml, b"<a k='v'>t</a>").unwrap(),
            "module.exports = ({\"a\":{\"@k\":\"v\",\"#text\":\"t\"}});\n"
        );
        assert_eq!(
            data_module_commonjs_source(DataFormat::Text, b"raw\n").unwrap(),
            "module.exports = (\"raw\\n\");\n"
        );
    }

    #[test]
    fn xml_is_decoded_from_its_own_bytes() {
        // A byte-order mark and a declared encoding settle the text, so the
        // loader hands over bytes rather than assuming UTF-8.
        let mut latin1 = b"<?xml version='1.0' encoding='ISO-8859-1'?><a k='".to_vec();
        latin1.push(0xE9);
        latin1.extend_from_slice(b"'/>");
        let source = data_module_source(DataFormat::Xml, &latin1).unwrap();
        assert!(source.contains("\u{e9}"), "{source}");
    }

    #[test]
    fn a_root_name_that_is_not_an_identifier_is_only_the_default_export() {
        let source = data_module_source(DataFormat::Xml, b"<x:feed/>").unwrap();
        assert_eq!(
            source,
            "const document = {\"x:feed\":\"\"};\nexport default document;\n"
        );
        let source = data_module_source(DataFormat::Xml, b"<class/>").unwrap();
        assert!(!source.contains("export const"), "{source}");
    }

    #[test]
    fn an_ill_formed_xml_file_is_reported_not_panicked() {
        let error = data_module_source(DataFormat::Xml, b"<a><b></a>").unwrap_err();
        assert!(error.message.contains("</b>"), "{}", error.message);
    }

    #[test]
    fn a_parse_failure_is_reported_not_panicked() {
        let error = data_module_source(DataFormat::Json, "{ not json".as_bytes()).unwrap_err();
        assert!(!error.message.is_empty());
    }

    #[test]
    fn nesting_is_bounded_before_a_parser_sees_the_text() {
        let deep = format!(
            "{}{}",
            "[".repeat(MAX_DATA_NESTING + 5),
            "]".repeat(MAX_DATA_NESTING + 5)
        );
        let error = data_module_source(DataFormat::Json, deep.as_bytes()).unwrap_err();
        assert!(error.message.contains("nests more than"));

        let fine = format!("{}{}", "[".repeat(8), "]".repeat(8));
        assert!(data_module_source(DataFormat::Json, fine.as_bytes()).is_ok());
    }

    #[test]
    fn delimiters_inside_strings_and_comments_do_not_count_as_nesting() {
        let brackets = "[".repeat(MAX_DATA_NESTING + 5);
        assert_eq!(nesting_depth_over(&format!("\"{brackets}\""), 8), None);
        assert_eq!(nesting_depth_over(&format!("// {brackets}\n"), 8), None);
        assert_eq!(nesting_depth_over(&format!("/* {brackets} */"), 8), None);
        assert_eq!(nesting_depth_over(&format!("# {brackets}\n"), 8), None);
        assert_eq!(nesting_depth_over("\"\\\"[[[[[[[[[[\"", 4), None);
    }
}
