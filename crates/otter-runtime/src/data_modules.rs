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
    /// Plain text, loaded as a string.
    Text,
}

impl DataFormat {
    /// Recognize a data format from a file extension.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        Some(match extension {
            "json" => Self::Json,
            "jsonc" => Self::Jsonc,
            "json5" => Self::Json5,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
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

/// Turn a data file's text into module source exporting its parsed value.
pub fn data_module_source(format: DataFormat, text: &str) -> Result<String, DataModuleError> {
    if format == DataFormat::Text {
        return Ok(export_default(&text));
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
        DataFormat::Text => unreachable!("text is handled before parsing"),
    };
    Ok(export_default(&value))
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

fn export_default(value: &impl Serialize) -> String {
    // A JSON literal is a JS expression, so the parsed document crosses into
    // the module graph as source rather than as a host value.
    let literal = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    format!("export default ({literal});\n")
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
        assert_eq!(DataFormat::from_extension("ts"), None);
    }

    #[test]
    fn yaml_and_toml_load_as_one_default_export() {
        assert_eq!(
            data_module_source(DataFormat::Yaml, "port: 8080\nhosts:\n  - a\n  - b\n").unwrap(),
            "export default ({\"port\":8080,\"hosts\":[\"a\",\"b\"]});\n"
        );
        assert_eq!(
            data_module_source(DataFormat::Toml, "port = 8080\nhosts = [\"a\"]\n").unwrap(),
            "export default ({\"hosts\":[\"a\"],\"port\":8080});\n"
        );
    }

    #[test]
    fn jsonc_and_json5_accept_what_json_rejects() {
        assert_eq!(
            data_module_source(DataFormat::Jsonc, "{\n // a comment\n \"a\": 1,\n}").unwrap(),
            "export default ({\"a\":1});\n"
        );
        assert_eq!(
            data_module_source(DataFormat::Json5, "{ a: 1, b: 'two' }").unwrap(),
            "export default ({\"a\":1,\"b\":\"two\"});\n"
        );
    }

    #[test]
    fn text_loads_as_a_string() {
        assert_eq!(
            data_module_source(DataFormat::Text, "line one\nline two\n").unwrap(),
            "export default (\"line one\\nline two\\n\");\n"
        );
    }

    #[test]
    fn a_parse_failure_is_reported_not_panicked() {
        let error = data_module_source(DataFormat::Json, "{ not json").unwrap_err();
        assert!(!error.message.is_empty());
    }

    #[test]
    fn nesting_is_bounded_before_a_parser_sees_the_text() {
        let deep = format!(
            "{}{}",
            "[".repeat(MAX_DATA_NESTING + 5),
            "]".repeat(MAX_DATA_NESTING + 5)
        );
        let error = data_module_source(DataFormat::Json, &deep).unwrap_err();
        assert!(error.message.contains("nests more than"));

        let fine = format!("{}{}", "[".repeat(8), "]".repeat(8));
        assert!(data_module_source(DataFormat::Json, &fine).is_ok());
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
