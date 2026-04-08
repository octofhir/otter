//! JavaScript-to-JavaScript transformation layer (currently: TS type stripping).
//!
//! For `.js`/`.mjs`/`.cjs` inputs this is a zero-allocation identity pass —
//! no parser, no transformer, no codegen, no second source map. For `.ts`/
//! `.mts`/`.cts` inputs we parse with oxc's TypeScript parser, run
//! `oxc_transformer` with the default TS preset to strip type annotations,
//! then `oxc_codegen` back to JavaScript with a V3 sourcemap.
//!
//! Either way we preserve the **original** source text so diagnostics render
//! exactly what the developer wrote (pre-strip for TS, verbatim for JS).

use std::sync::Arc;

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};

use crate::source::SourceLoweringError;

/// Result of lowering user-written source into the JavaScript the compiler
/// parses.
pub struct TransformedSource {
    /// JavaScript fed to the parser/compiler.
    pub generated_js: String,
    /// Original source text (TS or JS) the developer wrote.
    pub original: Arc<str>,
    /// V3 sourcemap from generated_js back to original. `None` for `.js`
    /// inputs (passthrough — generated_js == original).
    pub source_map: Option<oxc_sourcemap::SourceMap>,
    /// Display URL/path used for diagnostics.
    pub url: String,
    /// Whether the input was TypeScript.
    pub was_typescript: bool,
}

impl TransformedSource {
    /// Builds an identity passthrough `TransformedSource` for plain
    /// JavaScript. No parse, no transform, no codegen — just copy the
    /// source into the two buckets and return.
    #[must_use]
    pub fn identity(source: &str, source_url: &str) -> Self {
        Self {
            generated_js: source.to_string(),
            original: Arc::from(source),
            source_map: None,
            url: source_url.to_string(),
            was_typescript: false,
        }
    }
}

pub fn transform_source(
    source: &str,
    source_url: &str,
) -> Result<TransformedSource, SourceLoweringError> {
    let source_type = SourceType::from_path(source_url).unwrap_or_default();

    if source_type.is_jsx() {
        return Err(SourceLoweringError::Unsupported(
            "JSX and TSX are not supported. Please use pure JS or TS. (See CLI_REPORTER_PLAN.md)"
                .to_string(),
        ));
    }

    if source_type.is_typescript() {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, source_type).parse();

        if let Some(error) = parsed.errors.first() {
            return Err(SourceLoweringError::Parse(error.to_string()));
        }

        let transform_options = TransformOptions::default();
        let mut program = parsed.program;

        let semantic = SemanticBuilder::new().build(&program);
        let transformer = Transformer::new(
            &allocator,
            std::path::Path::new(source_url),
            &transform_options,
        );
        transformer.build_with_scoping(semantic.semantic.into_scoping(), &mut program);

        let codegen_options = CodegenOptions {
            source_map_path: Some(std::path::PathBuf::from(source_url)),
            ..CodegenOptions::default()
        };

        let codegen = Codegen::new().with_options(codegen_options).build(&program);

        Ok(TransformedSource {
            generated_js: codegen.code,
            original: Arc::from(source),
            source_map: codegen.map,
            url: source_url.to_string(),
            was_typescript: true,
        })
    } else {
        Ok(TransformedSource {
            generated_js: source.to_string(),
            original: Arc::from(source),
            source_map: None,
            url: source_url.to_string(),
            was_typescript: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_identity() {
        let transformed = transform_source("const x = 1;", "main.js").unwrap();
        assert!(transformed.source_map.is_none());
        assert!(!transformed.was_typescript);
        assert_eq!(transformed.generated_js, "const x = 1;");
        assert_eq!(&*transformed.original, "const x = 1;");
    }

    #[test]
    fn js_mjs_identity() {
        let transformed = transform_source("const x = 1;", "main.mjs").unwrap();
        assert!(transformed.source_map.is_none());
        assert!(!transformed.was_typescript);
    }

    #[test]
    fn js_cjs_identity() {
        let transformed = transform_source("const x = 1;", "main.cjs").unwrap();
        assert!(transformed.source_map.is_none());
        assert!(!transformed.was_typescript);
    }

    #[test]
    fn ts_strip_types() {
        let transformed = transform_source("const x: number = 1;", "main.ts").unwrap();
        assert!(transformed.source_map.is_some());
        assert!(transformed.was_typescript);
        // The generated js shouldn't have `: number`
        assert!(!transformed.generated_js.contains(": number"));
        // original should
        assert!(transformed.original.contains(": number"));

        let map = transformed.source_map.as_ref().unwrap();
        // Since we don't know the exact column right now without parsing it via the map
        // Let's at least check we have sourcemaps and mapping to tokens works without panic
        assert!(map.get_token(0).is_some());
    }

    #[test]
    fn ts_interface() {
        let src = "interface Foo { a: number }; const f: Foo = { a: 1 }; console.log(f.a);";
        let transformed = transform_source(src, "main.ts").unwrap();
        assert!(!transformed.generated_js.contains("interface Foo"));
        assert!(transformed.generated_js.contains("console.log(f.a);"));
        assert!(transformed.source_map.is_some());
    }
}
