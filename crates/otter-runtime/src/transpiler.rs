//! TypeScript to JavaScript transpiler.
//!
//! This module provides TypeScript transpilation using SWC (Speedy Web Compiler).
//! It strips type annotations and transforms TypeScript-specific syntax to JavaScript.

use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, sync::Lrc};
use swc_ecma_ast::{EsVersion, Program};
use swc_ecma_codegen::{Config as CodegenConfig, Emitter, text_writer::JsWriter};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_transforms_base::{fixer::fixer, resolver};
use swc_ecma_transforms_typescript::strip;
use swc_ecma_visit::VisitMutWith;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TranspileError {
    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Transform error: {0}")]
    Transform(String),

    #[error("Codegen error: {0}")]
    Codegen(String),
}

#[derive(Debug)]
pub struct TranspileResult {
    /// The transpiled JavaScript code.
    pub code: String,
    /// Source map (if generated).
    pub source_map: Option<String>,
}

/// Transpile TypeScript code to JavaScript.
pub fn transpile_typescript(source: &str) -> Result<TranspileResult, TranspileError> {
    transpile_typescript_with_options(source, TranspileOptions::default())
}

#[derive(Debug, Clone)]
pub struct TranspileOptions {
    /// Target ECMAScript version. Default: ES2020.
    pub target: EsVersion,
    /// Generate source map. Default: false.
    pub source_map: bool,
    /// File name for error messages. Default: "script.ts".
    pub filename: String,
}

impl Default for TranspileOptions {
    fn default() -> Self {
        Self {
            target: EsVersion::Es2020,
            source_map: false,
            filename: "script.ts".to_string(),
        }
    }
}

/// Transpile TypeScript code to JavaScript with custom options.
pub fn transpile_typescript_with_options(
    source: &str,
    options: TranspileOptions,
) -> Result<TranspileResult, TranspileError> {
    let cm: Lrc<SourceMap> = Default::default();

    let fm = cm.new_source_file(
        Lrc::new(FileName::Custom(options.filename.clone())),
        source.to_string(),
    );

    let syntax = Syntax::Typescript(TsSyntax {
        tsx: false,
        decorators: true,
        dts: false,
        no_early_errors: false,
        disallow_ambiguous_jsx_like: false,
    });

    let lexer = Lexer::new(syntax, options.target, StringInput::from(&*fm), None);

    let mut parser = Parser::new_from(lexer);

    let module = parser.parse_module().map_err(|e| {
        TranspileError::Parse(format!("Failed to parse TypeScript: {:?}", e.kind()))
    })?;

    for _e in parser.take_errors() {}

    let mut program = Program::Module(module);

    GLOBALS.set(&Globals::default(), || {
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();

        program.visit_mut_with(&mut resolver(unresolved_mark, top_level_mark, true));
        program.mutate(&mut strip(unresolved_mark, top_level_mark));
        program.visit_mut_with(&mut fixer(None));
    });

    let module = match program {
        Program::Module(m) => m,
        Program::Script(_) => {
            return Err(TranspileError::Transform(
                "Expected module, got script".to_string(),
            ));
        }
    };

    let mut buf = vec![];
    let mut src_map_buf = vec![];

    {
        let writer = JsWriter::new(
            cm.clone(),
            "\n",
            &mut buf,
            if options.source_map {
                Some(&mut src_map_buf)
            } else {
                None
            },
        );

        let codegen_config = CodegenConfig::default()
            .with_target(options.target)
            .with_ascii_only(false)
            .with_minify(false)
            .with_omit_last_semi(false);

        let mut emitter = Emitter {
            cfg: codegen_config,
            cm: cm.clone(),
            comments: None,
            wr: writer,
        };

        emitter
            .emit_module(&module)
            .map_err(|e| TranspileError::Codegen(format!("Failed to emit code: {}", e)))?;
    }

    let code = String::from_utf8(buf)
        .map_err(|e| TranspileError::Codegen(format!("Invalid UTF-8 output: {}", e)))?;

    let source_map = if options.source_map && !src_map_buf.is_empty() {
        let mut map_buf = vec![];
        cm.build_source_map(
            &src_map_buf,
            None,
            swc_common::source_map::DefaultSourceMapGenConfig,
        )
        .to_writer(&mut map_buf)
        .ok();
        String::from_utf8(map_buf).ok()
    } else {
        None
    };

    Ok(TranspileResult { code, source_map })
}

/// Check if code appears to be TypeScript (has type annotations).
///
/// This is a heuristic check - it looks for common TypeScript patterns.
pub fn is_typescript(source: &str) -> bool {
    let ts_patterns = [
        ": string",
        ": number",
        ": boolean",
        ": any",
        ": void",
        ": null",
        ": undefined",
        ": never",
        ": unknown",
        ": object",
        "interface ",
        "type ",
        "enum ",
        "as const",
        "as string",
        "as number",
        "<T>",
        "<T,",
        ": T",
        "readonly ",
        "private ",
        "protected ",
        "public ",
        "implements ",
        "extends ",
        "namespace ",
        "declare ",
        "?:",
        "!:",
    ];

    for pattern in ts_patterns {
        if source.contains(pattern) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transpile_simple_typescript() {
        let ts_code = r#"
            const name: string = "test";
            const count: number = 42;
            console.log(name, count);
        "#;

        let result = transpile_typescript(ts_code).expect("transpile failed");

        assert!(!result.code.contains(": string"));
        assert!(!result.code.contains(": number"));
        assert!(result.code.contains("const name"));
        assert!(result.code.contains("const count"));
    }

    #[test]
    fn test_transpile_interface_and_type() {
        let ts_code = r#"
            interface Patient {
                id: string;
                name: string;
            }

            type Status = "active" | "inactive";

            const patient: Patient = { id: "123", name: "John" };
            const status: Status = "active";
        "#;

        let result = transpile_typescript(ts_code).expect("transpile failed");

        assert!(!result.code.contains("interface Patient"));
        assert!(!result.code.contains("type Status"));
        assert!(result.code.contains("const patient"));
    }

    #[test]
    fn test_transpile_generics() {
        let ts_code = r#"
            function identity<T>(value: T): T {
                return value;
            }

            const result = identity<string>("hello");
        "#;

        let result = transpile_typescript(ts_code).expect("transpile failed");

        assert!(!result.code.contains("<T>"));
        assert!(!result.code.contains("<string>"));
        assert!(result.code.contains("function identity"));
    }

    #[test]
    fn test_transpile_preserves_javascript() {
        let js_code = r#"
            const name = "test";
            const count = 42;
            console.log(name, count);
        "#;

        let result = transpile_typescript(js_code).expect("transpile failed");

        assert!(result.code.contains("const name"));
        assert!(result.code.contains("const count"));
    }

    #[test]
    fn test_transpile_error_handling() {
        let invalid_ts = r#"
            const x: = "invalid syntax";
        "#;

        let result = transpile_typescript(invalid_ts);
        assert!(result.is_err());
    }
}
