//! Compose generated-JS spans back to the original-source `(line, column)`.
//!
//! For a `.js` script the "generated JS" is simply the original JS, and the
//! JS↔original map is `None` (identity). For a `.ts` script the generated JS
//! is the oxc-codegen output and the map is the V3 sourcemap that oxc emitted.
//!
//! The compiler feeds this mapper its AST spans (which are byte offsets into
//! the generated JS). The mapper returns a `SourceLocation` expressed in the
//! *original* source (TS or JS) 1-based line and column — those end up stored
//! on the VM `SourceMap` so runtime errors point at what the user wrote.

use oxc_sourcemap::SourceMap as OxcSourceMap;
use oxc_span::Span;

use crate::source_compiler::line_index::SourceLineIndex;
use crate::source_map::SourceLocation;

/// Composes generated-JS byte offsets into original-source locations.
///
/// Holds both halves of the composition:
///   1. A `SourceLineIndex` built from the generated JS, for the first hop.
///   2. An optional oxc V3 sourcemap, for the second hop.
///
/// When the second hop is absent (`.js` inputs), the mapper returns generated
/// locations unchanged — which is exactly what we want, because for `.js`
/// files the "generated JS" *is* the original source.
#[derive(Debug)]
pub struct SourceMapper {
    line_index: SourceLineIndex,
    oxc_map: Option<OxcSourceMap>,
}

impl SourceMapper {
    /// Builds a mapper for a plain `.js` source (identity second hop).
    #[must_use]
    pub fn identity(generated_js: &str) -> Self {
        Self {
            line_index: SourceLineIndex::new(generated_js),
            oxc_map: None,
        }
    }

    /// Builds a mapper that composes the generated-JS line index with an
    /// oxc V3 sourcemap (for `.ts` inputs).
    #[must_use]
    pub fn with_oxc_map(generated_js: &str, oxc_map: OxcSourceMap) -> Self {
        Self {
            line_index: SourceLineIndex::new(generated_js),
            oxc_map: Some(oxc_map),
        }
    }

    /// Returns the original-source `(line, column)` for a generated-JS span.
    ///
    /// Uses the span's **start** byte as the anchor. If the span is unspanned
    /// (a synthesized node with no real source position), falls back to
    /// `SourceLocation::new(1, 1)`.
    #[must_use]
    pub fn locate(&self, span: Span) -> SourceLocation {
        if span.is_unspanned() {
            return SourceLocation::new(1, 1);
        }

        let generated = self.line_index.locate(span.start);

        let Some(map) = self.oxc_map.as_ref() else {
            return generated;
        };

        // oxc's sourcemap is 0-based, ours is 1-based. Convert in both
        // directions across the API boundary.
        let gen_line_0 = generated.line().saturating_sub(1);
        let gen_col_0 = generated.column().saturating_sub(1);

        let lookup = map.generate_lookup_table();
        let Some(token) = map.lookup_token(&lookup, gen_line_0, gen_col_0) else {
            return generated;
        };

        SourceLocation::new(
            token.get_src_line().saturating_add(1),
            token.get_src_col().saturating_add(1),
        )
    }

    /// Returns the underlying generated-JS line index.
    #[must_use]
    #[allow(dead_code)] // Used by future diagnostic rendering helpers.
    pub fn line_index(&self) -> &SourceLineIndex {
        &self.line_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_locates_span_start() {
        let src = "let x = 1;\nthrow x;";
        let mapper = SourceMapper::identity(src);
        // "throw" starts at byte 11 (after "let x = 1;\n"), line 2 col 1.
        let span = Span::new(11, 16);
        let loc = mapper.locate(span);
        assert_eq!(loc.line(), 2);
        assert_eq!(loc.column(), 1);
    }

    #[test]
    fn identity_unspanned_is_one_one() {
        let mapper = SourceMapper::identity("let x = 1;");
        let loc = mapper.locate(oxc_span::SPAN);
        assert_eq!(loc.line(), 1);
        assert_eq!(loc.column(), 1);
    }
}
