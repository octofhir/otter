//! Immutable startup sources with an independently validated first result.
//!
//! # Contents
//! - The three predeclared empty/JavaScript/TypeScript startup cases.
//! - OXC source spans that evaluate the original final expression exactly once.
//! - Owned source identities and the exact single-line stdout contract.
//!
//! # Invariants
//! - Original hashes are fixed before measurements; workload bytes are retained.
//! - TypeScript remains TypeScript and goes through each engine's real pipeline.
//! - The common wrapper validates the result before printing its marker.
//! - The external observer owns clocks; no engine clock or VM handle escapes.
//!
//! # See also
//! - `scripts/dev/first-result.py` measures validated parent-observed delivery.
//! - `crate::warm_harness` owns the persistent seven-anchor workload protocol.

use otter_syntax::{SourceGoal, SourceKind, with_program_goal};
use oxc_ast::ast::Statement;
use oxc_span::GetSpan;
use serde::{Deserialize, Serialize};

use crate::warm_harness::{WarmPieceRole, WarmSourcePiece, source_sha256};

/// Required startup workloads, retained together even if an engine is slower.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StartupCase {
    /// Empty source through the actual runtime, followed by the common marker.
    EmptyCli,
    /// The original `undefined;` Script.
    TinyJs,
    /// The original typed declaration and observable final expression.
    TinyTs,
}

impl StartupCase {
    /// All required cases in deterministic order.
    pub const ALL: [Self; 3] = [Self::EmptyCli, Self::TinyJs, Self::TinyTs];

    /// Current case name, also used for generated filenames.
    pub const fn name(self) -> &'static str {
        match self {
            Self::EmptyCli => "empty-cli",
            Self::TinyJs => "tiny-js",
            Self::TinyTs => "tiny-ts",
        }
    }

    /// Predeclared complete original content identity.
    pub const fn original_sha256(self) -> &'static str {
        match self {
            Self::EmptyCli => "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            Self::TinyJs => "97fc53259bfb5a570a4eb6cd8a34e11a0e03261810020ba16410258a020df6fe",
            Self::TinyTs => "8f2185c37bb27abf1c95decc30df7da467398f6d8a7c57b4846f88f53c6ad4f2",
        }
    }

    /// Generated filename preserving the actual source language.
    pub fn filename(self) -> String {
        format!(
            "{}.{}",
            self.name(),
            if self == Self::TinyTs { "ts" } else { "js" }
        )
    }
}

/// Owned proof of the original source and its complete stdout contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartupManifest {
    /// Exact required case.
    pub case: StartupCase,
    /// Original source path or the explicit empty-source identity.
    pub source_name: String,
    /// Complete original content hash, fixed independently of generated work.
    pub original_sha256: String,
    /// Exact generated source hash.
    pub generated_sha256: String,
    /// Original final expression, evaluated once by the wrapper.
    pub result: Option<WarmSourcePiece>,
    /// Whole successful stdout, including its final newline.
    pub expected_stdout: String,
    /// Disclosed common wrapper and observer measurement boundary.
    pub scope: String,
}

/// One generated source plus its owned validation contract.
#[derive(Debug, Clone)]
pub struct PreparedStartup {
    /// Shared bytes consumed by each engine, preserving the source language.
    pub source: String,
    /// Identities and independent result validation.
    pub manifest: StartupManifest,
}

/// Prepare a required immutable startup fixture without executing any engine.
pub fn prepare_startup_harness(
    case: StartupCase,
    source_name: String,
    original: &str,
) -> Result<PreparedStartup, String> {
    let original_hash = source_sha256(original.as_bytes());
    if original_hash != case.original_sha256() {
        return Err(format!(
            "{}: original startup content identity changed",
            case.name()
        ));
    }
    let (source, result) = if case == StartupCase::EmptyCli {
        ("const __rfFirstResult = undefined;\n".to_owned(), None)
    } else {
        let kind = if case == StartupCase::TinyTs {
            SourceKind::TypeScript
        } else {
            SourceKind::JavaScript
        };
        with_program_goal(original, kind, SourceGoal::Script, |program| {
            if program.source_text != original
                || !program.directives.is_empty()
                || program.hashbang.is_some()
            {
                return Err("startup span copying requires the exact original Script".to_owned());
            }
            let Some(Statement::ExpressionStatement(statement)) = program.body.last() else {
                return Err("startup source must end with its observable expression".to_owned());
            };
            let expression = statement.expression.span();
            let statement_span = statement.span();
            let text = original
                .get(expression.start as usize..expression.end as usize)
                .ok_or_else(|| "invalid original result span".to_owned())?;
            let prefix = original
                .get(..statement_span.start as usize)
                .ok_or_else(|| "invalid original prefix span".to_owned())?;
            let suffix = original
                .get(statement_span.end as usize..)
                .ok_or_else(|| "invalid original suffix span".to_owned())?;
            let source = format!("{prefix}const __rfFirstResult = ({text});{suffix}\n");
            Ok((
                source,
                Some(WarmSourcePiece {
                    role: WarmPieceRole::Result,
                    start: expression.start,
                    end: expression.end,
                    sha256: source_sha256(text.as_bytes()),
                }),
            ))
        })
        .map_err(|error| format!("startup Script parse: {error:?}"))??
    };
    let expected = if case == StartupCase::TinyTs {
        "1"
    } else {
        "undefined"
    };
    let marker = format!("fixed-first-result\t{}\t{expected}", case.name());
    let quoted = serde_json::to_string(&marker).map_err(|error| error.to_string())?;
    let source = format!(
        "{source}if (__rfFirstResult !== {expected}) throw new Error(\"startup result mismatch\");\nconsole.log({quoted});\n"
    );
    let manifest = StartupManifest {
        case, source_name, original_sha256: original_hash,
        generated_sha256: source_sha256(source.as_bytes()), result,
        expected_stdout: format!("{marker}\n"),
        scope: "parent-observed complete validated stdout line; launch, original work, common validation/marker and transport included".into(),
    };
    Ok(PreparedStartup { source, manifest })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_required_sources_preserve_real_language_and_one_original_result() {
        let originals = [
            "",
            include_str!("../../../benchmarks/fixtures/engine/tiny.js"),
            include_str!("../../../benchmarks/fixtures/engine/tiny.ts"),
        ];
        for (case, original) in StartupCase::ALL.into_iter().zip(originals) {
            let prepared = prepare_startup_harness(case, "fixture".into(), original).unwrap();
            assert_eq!(
                source_sha256(prepared.source.as_bytes()),
                prepared.manifest.generated_sha256
            );
            assert_eq!(prepared.manifest.original_sha256, case.original_sha256());
            assert_eq!(prepared.source.matches("console.log(").count(), 1);
            assert!(prepared.manifest.expected_stdout.ends_with('\n'));
            if let Some(result) = &prepared.manifest.result {
                let expression = &original[result.start as usize..result.end as usize];
                assert_eq!(source_sha256(expression.as_bytes()), result.sha256);
                assert!(
                    prepared
                        .source
                        .contains(&format!("const __rfFirstResult = ({expression});"))
                );
            }
            if case == StartupCase::TinyTs {
                assert!(case.filename().ends_with(".ts"));
                assert!(prepared.source.starts_with("const value: number = 1;\n"));
                assert_eq!(
                    prepared.manifest.expected_stdout,
                    "fixed-first-result\ttiny-ts\t1\n"
                );
            }
        }
    }

    #[test]
    fn changed_or_reduced_original_is_rejected_before_preparation() {
        for case in StartupCase::ALL {
            assert!(prepare_startup_harness(case, "fixture".into(), "1;\n").is_err());
        }
    }
}
