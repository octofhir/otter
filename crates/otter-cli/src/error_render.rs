//! Human and JSON error rendering for the `otter` CLI.
//!
//! # Contents
//! - [`emit_error`] — top-level CLI error renderer.
//! - OXC/miette conversion for runtime diagnostics.
//!
//! # Invariants
//! - JSON output stays the stable `OtterError` wire shape.
//! - Human output uses OXC's miette fork so parser/compiler/runtime diagnostics
//!   render through the same report handler.
//!
//! # See also
//! - <https://github.com/oxc-project/oxc-miette>

use std::path::PathBuf;

use otter_runtime::{Diagnostic, OtterError};
use oxc_diagnostics::{GraphicalReportHandler, LabeledSpan, NamedSource, OxcDiagnostic};

/// Emit a CLI error to stderr.
pub(crate) fn emit_error(err: &OtterError, json: bool) {
    if json {
        match err.to_json() {
            Ok(s) => eprintln!("{s}"),
            Err(_) => eprintln!("error: {err}"),
        }
        return;
    }

    if !emit_human_diagnostics(err) {
        eprintln!("error: {err}");
    }
}

fn emit_human_diagnostics(err: &OtterError) -> bool {
    match err {
        OtterError::Compile { diagnostics } => {
            for diagnostic in diagnostics {
                emit_human_diagnostic(diagnostic);
            }
            true
        }
        OtterError::Runtime { diagnostic } => {
            emit_human_diagnostic(diagnostic);
            true
        }
        _ => false,
    }
}

fn emit_human_diagnostic(diagnostic: &Diagnostic) {
    match render_human_diagnostic(diagnostic) {
        Some(rendered) => eprint!("{rendered}"),
        None => eprintln!("error[{}]: {}", diagnostic.code, diagnostic.message),
    }
}

fn render_human_diagnostic(diagnostic: &Diagnostic) -> Option<String> {
    let mut report = OxcDiagnostic::error(diagnostic.message.clone())
        .with_error_code("otter", diagnostic.code.clone());
    if let Some(help) = &diagnostic.help {
        report = report.with_help(help.clone());
    }
    if let Some(range) = diagnostic.range.or(diagnostic.span) {
        report = report.with_label(LabeledSpan::new_primary_with_span(
            Some(diagnostic.code.clone()),
            // miette ≥3.0 spans are `u32` byte offsets; `range` is already `(u32, u32)`.
            range.0..range.1,
        ));
    }

    let mut rendered = String::new();
    let handler = GraphicalReportHandler::new().with_links(false);
    let render_result = if let Some((name, source)) =
        diagnostic.source_url.as_deref().and_then(read_named_source)
    {
        let named = NamedSource::new(name, source);
        let report = report.with_source_code(named);
        handler.render_report(&mut rendered, report.as_ref())
    } else {
        handler.render_report(&mut rendered, &report)
    };

    render_result.ok().map(|()| rendered)
}

fn read_named_source(source_url: &str) -> Option<(String, String)> {
    let path = path_from_source_url(source_url)?;
    let source = std::fs::read_to_string(&path).ok()?;
    Some((path.display().to_string(), source))
}

fn path_from_source_url(source_url: &str) -> Option<PathBuf> {
    if source_url.starts_with('<') {
        return None;
    }
    source_url
        .strip_prefix("file://")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(source_url)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_runtime::Diagnostic;

    #[test]
    fn renders_runtime_diagnostic_with_oxc_miette_snippet() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("entry.ts");
        std::fs::write(&path, "const = ;\n").expect("write source");

        let rendered = render_human_diagnostic(
            &Diagnostic::syntax("expected an identifier")
                .with_source_url(path.display().to_string())
                .with_range((6, 7))
                .with_help("fix the syntax error in the source file"),
        )
        .expect("render");

        assert!(rendered.contains("expected an identifier"));
        assert!(rendered.contains("entry.ts"));
        assert!(rendered.contains("fix the syntax error"));
    }
}
