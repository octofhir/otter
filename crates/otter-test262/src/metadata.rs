//! YAML frontmatter parser for Test262 tests.
//!
//! Every Test262 file opens with a `/*--- ... ---*/` block carrying
//! a YAML document that the runner uses to filter on `features:` /
//! `flags:`, drive `negative:` inversion, and decide whether to
//! route the test as a script or a module. This module isolates the
//! parser so the per-test driver in [`crate::runner`] can stay
//! mechanical.
//!
//! Spec: <https://github.com/tc39/test262/blob/main/INTERPRETING.md#metadata>
//!
//! Frontmatter shape (informal):
//!
//! ```text
//! /*---
//! description: ...
//! esid: sec-...
//! info: |
//!   ...
//! features: [BigInt, Atomics]
//! flags: [module]
//! includes: [propertyHelper.js]
//! negative:
//!   phase: parse
//!   type: SyntaxError
//! ---*/
//! ```
//!
//! Test262's YAML is loose — multiline strings beginning with `|`,
//! top-level keys without quotes, comments — and the parser accepts
//! every shape the corpus actually carries.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Parsed `/*--- ... ---*/` frontmatter.
///
/// Spec link: <https://github.com/tc39/test262/blob/main/INTERPRETING.md#metadata>.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Frontmatter {
    /// Free-form description (surfaced in failure reports).
    pub description: Option<String>,
    /// ECMA-262 spec anchor (`sec-array.from`, etc.).
    pub esid: Option<String>,
    /// `es5id` / `es6id` are legacy fields the corpus still carries.
    pub es5id: Option<String>,
    /// `es6id` (legacy).
    pub es6id: Option<String>,
    /// Free-form supplementary block (kept for reports).
    pub info: Option<String>,
    /// Required ECMAScript proposal / feature tokens.
    pub features: Vec<String>,
    /// Test flags (raw strings from YAML; parsed into [`TestFlag`]
    /// via [`Frontmatter::test_flags`]).
    pub flags: Vec<String>,
    /// Locale list for ECMA-402 tests.
    pub locale: Vec<String>,
    /// Harness fragments to load before the test body.
    pub includes: Vec<String>,
    /// Negative-test expectation.
    pub negative: Option<Negative>,
    /// `author:` + other free-form fields are accepted but ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// Negative-test expectation per
/// [INTERPRETING §negative](https://github.com/tc39/test262/blob/main/INTERPRETING.md#negative).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Negative {
    /// When the error is expected to fire.
    pub phase: NegativePhase,
    /// Expected error class name (matched against `value.name`).
    #[serde(rename = "type")]
    pub type_: String,
}

/// Phase at which a negative test expects an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NegativePhase {
    /// Parse-time / static-semantics error.
    Parse,
    /// Static early error (per ECMA-262 §5.3 Static Semantics: Early
    /// Errors). For grading purposes we treat `early` ≡ `parse`.
    Early,
    /// Module link / linker error.
    Resolution,
    /// Runtime exception during execution.
    Runtime,
}

impl NegativePhase {
    /// Promote `early` to `parse` for grading. The §41 closeout
    /// surfaces both as the same `CompileError` path.
    #[must_use]
    pub fn canonical(self) -> Self {
        match self {
            Self::Early => Self::Parse,
            other => other,
        }
    }
}

/// Strongly-typed form of every flag the corpus carries today.
///
/// Spec: <https://github.com/tc39/test262/blob/main/INTERPRETING.md#flags>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TestFlag {
    /// Strict-mode only.
    OnlyStrict,
    /// Sloppy-mode only.
    NoStrict,
    /// ES module entry point.
    Module,
    /// No harness preamble — execute the source as-is.
    Raw,
    /// `$DONE` polyfill required.
    Async,
    /// Generated test (codegen artifact).
    Generated,
    /// Host claims `[[CanBlock]] = false`.
    CanBlockIsFalse,
    /// Host claims `[[CanBlock]] = true`.
    CanBlockIsTrue,
    /// Test result is non-deterministic by design.
    NonDeterministic,
}

impl TestFlag {
    /// Best-effort decode of a single YAML token. Named with the
    /// `parse_token` prefix to avoid colliding with the
    /// `std::str::FromStr` trait shape — this is fallible-by-design
    /// (returns `None` on unknown tokens, not `Err`).
    #[must_use]
    pub fn parse_token(token: &str) -> Option<Self> {
        match token {
            "onlyStrict" => Some(Self::OnlyStrict),
            "noStrict" => Some(Self::NoStrict),
            "module" => Some(Self::Module),
            "raw" => Some(Self::Raw),
            "async" => Some(Self::Async),
            "generated" => Some(Self::Generated),
            "CanBlockIsFalse" => Some(Self::CanBlockIsFalse),
            "CanBlockIsTrue" => Some(Self::CanBlockIsTrue),
            "non-deterministic" => Some(Self::NonDeterministic),
            _ => None,
        }
    }
}

/// Errors raised while parsing frontmatter.
#[derive(Debug, Error)]
pub enum FrontmatterError {
    /// Source has no `/*--- ... ---*/` block.
    #[error("test source is missing the /*--- ... ---*/ frontmatter block")]
    MissingBlock,
    /// `/*---` was found but the matching `---*/` never closed.
    #[error("frontmatter block was opened but never closed")]
    UnclosedBlock,
    /// Underlying YAML decode error.
    #[error("frontmatter YAML is invalid: {0}")]
    Yaml(String),
}

impl Frontmatter {
    /// Parse the `/*--- ... ---*/` block from a test source.
    ///
    /// # Errors
    /// - [`FrontmatterError::MissingBlock`] when no `/*---` opener
    ///   is found.
    /// - [`FrontmatterError::UnclosedBlock`] when the opener has no
    ///   matching `---*/`.
    /// - [`FrontmatterError::Yaml`] on YAML decode failure.
    pub fn parse(source: &str) -> Result<Self, FrontmatterError> {
        let yaml = extract_yaml(source)?;
        // `\r` lurks in some Windows-style files; normalise so
        // serde_yaml never sees `\r\n`.
        let normalised = yaml.replace('\r', "");
        serde_yaml::from_str(&normalised).map_err(|e| FrontmatterError::Yaml(e.to_string()))
    }

    /// Extract the YAML body without parsing it. Useful for tooling.
    ///
    /// # Errors
    /// Same shape as [`Frontmatter::parse`] minus YAML decode.
    pub fn extract_block(source: &str) -> Result<String, FrontmatterError> {
        Ok(extract_yaml(source)?.to_string())
    }

    /// Return the source body that follows the closing `---*/`.
    /// The runner concatenates the harness preamble + this body.
    #[must_use]
    pub fn body_of(source: &str) -> &str {
        match source.find("---*/") {
            Some(end) => &source[end + 5..],
            None => source,
        }
    }

    /// Decode every recognised [`TestFlag`] from `flags:`. Unknown
    /// tokens are silently dropped (the corpus does occasionally
    /// carry experimental flags the runner doesn't grade).
    #[must_use]
    pub fn test_flags(&self) -> Vec<TestFlag> {
        self.flags
            .iter()
            .filter_map(|s| TestFlag::parse_token(s))
            .collect()
    }

    /// Convenience: any `flags:` token literally equal to `name`.
    #[must_use]
    pub fn has_flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    /// `flags: [onlyStrict]`.
    #[must_use]
    pub fn is_only_strict(&self) -> bool {
        self.has_flag("onlyStrict")
    }
    /// `flags: [noStrict]`.
    #[must_use]
    pub fn is_no_strict(&self) -> bool {
        self.has_flag("noStrict")
    }
    /// `flags: [module]`.
    #[must_use]
    pub fn is_module(&self) -> bool {
        self.has_flag("module")
    }
    /// `flags: [raw]`.
    #[must_use]
    pub fn is_raw(&self) -> bool {
        self.has_flag("raw")
    }
    /// `flags: [async]`.
    #[must_use]
    pub fn is_async(&self) -> bool {
        self.has_flag("async")
    }
}

/// Slice the YAML body out of a Test262 source.
///
/// The block opens with `/*---` and closes with `---*/`. INTERPRETING.md
/// guarantees the opener is on its own line, but the runner does not
/// rely on that — substring search across the whole source is safer.
fn extract_yaml(source: &str) -> Result<&str, FrontmatterError> {
    let start = source.find("/*---").ok_or(FrontmatterError::MissingBlock)?;
    let after_open = &source[start + 5..];
    let close_rel = after_open
        .find("---*/")
        .ok_or(FrontmatterError::UnclosedBlock)?;
    Ok(&after_open[..close_rel])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_block() {
        let src = "/*---\ndescription: A minimal test\n---*/\n1 + 1;";
        let fm = Frontmatter::parse(src).unwrap();
        assert_eq!(fm.description.as_deref(), Some("A minimal test"));
        assert!(fm.flags.is_empty());
        assert!(fm.negative.is_none());
    }

    #[test]
    fn parses_features_and_flags() {
        let src = "// hi\n/*---\nesid: sec-foo\nfeatures: [BigInt, Atomics]\nflags: [onlyStrict, module]\n---*/\nx;";
        let fm = Frontmatter::parse(src).unwrap();
        assert_eq!(fm.esid.as_deref(), Some("sec-foo"));
        assert_eq!(fm.features, vec!["BigInt", "Atomics"]);
        assert!(fm.is_only_strict());
        assert!(fm.is_module());
        assert!(!fm.is_no_strict());
        let typed = fm.test_flags();
        assert!(typed.contains(&TestFlag::OnlyStrict));
        assert!(typed.contains(&TestFlag::Module));
    }

    #[test]
    fn parses_negative_with_phase_runtime() {
        let src = "/*---\nnegative:\n  phase: runtime\n  type: TypeError\n---*/\n";
        let fm = Frontmatter::parse(src).unwrap();
        let neg = fm.negative.unwrap();
        assert_eq!(neg.phase, NegativePhase::Runtime);
        assert_eq!(neg.type_, "TypeError");
    }

    #[test]
    fn parses_negative_with_phase_parse_and_early_canonicalises() {
        let src = "/*---\nnegative:\n  phase: parse\n  type: SyntaxError\n---*/\n";
        let fm = Frontmatter::parse(src).unwrap();
        assert_eq!(fm.negative.as_ref().unwrap().phase, NegativePhase::Parse);

        let early = "/*---\nnegative:\n  phase: early\n  type: SyntaxError\n---*/\n";
        let fm2 = Frontmatter::parse(early).unwrap();
        assert_eq!(
            fm2.negative.as_ref().unwrap().phase.canonical(),
            NegativePhase::Parse
        );
    }

    #[test]
    fn parses_negative_resolution() {
        let src = "/*---\nnegative:\n  phase: resolution\n  type: SyntaxError\n---*/\n";
        let fm = Frontmatter::parse(src).unwrap();
        assert_eq!(
            fm.negative.as_ref().unwrap().phase,
            NegativePhase::Resolution
        );
    }

    #[test]
    fn parses_multiline_info_with_pipe() {
        let src =
            "/*---\ndescription: hi\ninfo: |\n  line one\n  line two\nfeatures: [Symbol]\n---*/\n";
        let fm = Frontmatter::parse(src).unwrap();
        assert!(fm.info.unwrap().contains("line one"));
        assert_eq!(fm.features, vec!["Symbol"]);
    }

    #[test]
    fn parses_includes() {
        let src = "/*---\nincludes: [propertyHelper.js, sta.js]\n---*/\n";
        let fm = Frontmatter::parse(src).unwrap();
        assert_eq!(fm.includes, vec!["propertyHelper.js", "sta.js"]);
    }

    #[test]
    fn body_of_strips_frontmatter() {
        let src = "// preamble\n/*---\ndescription: x\n---*/\nconst x = 1;\n";
        assert_eq!(Frontmatter::body_of(src), "\nconst x = 1;\n");
    }

    #[test]
    fn missing_block_errors() {
        assert!(matches!(
            Frontmatter::parse("// no frontmatter here"),
            Err(FrontmatterError::MissingBlock)
        ));
    }

    #[test]
    fn unclosed_block_errors() {
        assert!(matches!(
            Frontmatter::parse("/*---\ndescription: x"),
            Err(FrontmatterError::UnclosedBlock)
        ));
    }

    #[test]
    fn missing_trailing_newline_still_parses() {
        // No newline before `---*/`, no newline at EOF.
        let src = "/*---\ndescription: ok---*/";
        // YAML-side this is awkward but the YAML parser tolerates it
        // because `description` becomes `"ok"` (trailing markers
        // strip).
        let fm = Frontmatter::parse(src);
        // Best-effort: either it parses, or it surfaces a YAML error.
        // The important contract is "no panic".
        match fm {
            Ok(_) | Err(FrontmatterError::Yaml(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn every_flag_token_round_trips() {
        let combos = [
            ("onlyStrict", TestFlag::OnlyStrict),
            ("noStrict", TestFlag::NoStrict),
            ("module", TestFlag::Module),
            ("raw", TestFlag::Raw),
            ("async", TestFlag::Async),
            ("generated", TestFlag::Generated),
            ("CanBlockIsFalse", TestFlag::CanBlockIsFalse),
            ("CanBlockIsTrue", TestFlag::CanBlockIsTrue),
            ("non-deterministic", TestFlag::NonDeterministic),
        ];
        for (token, want) in combos {
            assert_eq!(TestFlag::parse_token(token), Some(want), "for {token}");
        }
        assert_eq!(TestFlag::parse_token("unknown"), None);
    }
}
