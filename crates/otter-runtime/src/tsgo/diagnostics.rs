//! Diagnostic types from TypeScript type checking.
//!
//! This module defines the types used to represent diagnostics (errors, warnings, etc.)
//! returned by the tsgo type checker.

use serde::{Deserialize, Serialize};

/// Position in a source file.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub line: u64,
    pub character: u64,
}

/// Diagnostic severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    /// A type error that prevents successful compilation.
    Error,
    /// A warning that doesn't prevent compilation.
    Warning,
    /// A suggestion for code improvement.
    Suggestion,
    /// An informational message.
    Message,
}

impl DiagnosticSeverity {
    /// Convert to display string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Suggestion => "suggestion",
            Self::Message => "message",
        }
    }

    /// Create from tsgo category string.
    pub fn from_category(category: &str) -> Self {
        match category {
            "error" => Self::Error,
            "warning" => Self::Warning,
            "suggestion" => Self::Suggestion,
            "message" => Self::Message,
            _ => Self::Error, // Default to error for unknown categories
        }
    }
}

impl std::fmt::Display for DiagnosticSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A diagnostic message from type checking (tsgo format).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    /// Source file path.
    #[serde(default)]
    pub file_name: String,

    /// Start position in the source file.
    #[serde(default)]
    pub start: Position,

    /// End position in the source file.
    #[serde(default)]
    pub end: Position,

    /// Start offset in the file.
    #[serde(default)]
    pub start_pos: u32,

    /// End offset in the file.
    #[serde(default)]
    pub end_pos: u32,

    /// TypeScript error code (numeric).
    #[serde(default)]
    pub code: u32,

    /// Category: "error", "warning", "suggestion", "message".
    #[serde(default)]
    pub category: String,

    /// Human-readable error message.
    #[serde(default)]
    pub message: String,

    /// Message chain for detailed errors.
    #[serde(default)]
    pub message_chain: Vec<Diagnostic>,

    /// Related diagnostics (e.g., "see declaration here").
    #[serde(default)]
    pub related_information: Vec<Diagnostic>,

    /// Whether this diagnostic reports an unnecessary item.
    #[serde(default)]
    pub reports_unnecessary: bool,

    /// Whether this diagnostic reports a deprecated item.
    #[serde(default)]
    pub reports_deprecated: bool,

    /// Whether this diagnostic was skipped on noEmit.
    #[serde(default)]
    pub skipped_on_no_emit: bool,

    /// The source line where the error occurred.
    #[serde(default)]
    pub source_line: String,
}

impl Diagnostic {
    /// Get the severity of this diagnostic.
    pub fn severity(&self) -> DiagnosticSeverity {
        DiagnosticSeverity::from_category(&self.category)
    }

    /// Check if this diagnostic is an error.
    pub fn is_error(&self) -> bool {
        self.category == "error"
    }

    /// Check if this diagnostic is a warning.
    pub fn is_warning(&self) -> bool {
        self.category == "warning"
    }

    /// Get the line number (1-indexed for display).
    pub fn line(&self) -> u64 {
        self.start.line + 1
    }

    /// Get the column number (1-indexed for display).
    pub fn column(&self) -> u64 {
        self.start.character + 1
    }

    /// Format the TypeScript error code (e.g., "TS2304").
    pub fn ts_code(&self) -> String {
        format!("TS{}", self.code)
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}({}:{}): {} {}: {}",
            self.file_name,
            self.line(),
            self.column(),
            self.category,
            self.ts_code(),
            self.message
        )
    }
}

/// Check if any diagnostics contain errors.
pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(|d| d.is_error())
}

/// Count the number of errors in diagnostics.
pub fn error_count(diagnostics: &[Diagnostic]) -> usize {
    diagnostics.iter().filter(|d| d.is_error()).count()
}

/// Count the number of warnings in diagnostics.
pub fn warning_count(diagnostics: &[Diagnostic]) -> usize {
    diagnostics.iter().filter(|d| d.is_warning()).count()
}

/// Format diagnostics for display.
///
/// Produces output similar to tsc:
/// ```text
/// src/main.ts(10:5): error TS2304: Cannot find name 'foo'.
///   src/types.ts(5:1): 'foo' is declared here.
/// ```
pub fn format_diagnostics(diagnostics: &[Diagnostic]) -> String {
    let mut output = String::new();

    for diag in diagnostics {
        output.push_str(&format!(
            "{}({}:{}): {} {}: {}\n",
            diag.file_name,
            diag.line(),
            diag.column(),
            diag.category,
            diag.ts_code(),
            diag.message
        ));

        for related in &diag.related_information {
            output.push_str(&format!(
                "  {}({}:{}): {}\n",
                related.file_name,
                related.line(),
                related.column(),
                related.message
            ));
        }
    }

    output
}

/// Format diagnostics with ANSI color codes for terminal display.
///
/// Uses standard ANSI escape codes that work in most modern terminals.
pub fn format_diagnostics_colored(diagnostics: &[Diagnostic]) -> String {
    use std::fmt::Write;

    let mut output = String::new();

    for diag in diagnostics {
        let severity_color = match diag.category.as_str() {
            "error" => "\x1b[31m",      // Red
            "warning" => "\x1b[33m",    // Yellow
            "suggestion" => "\x1b[36m", // Cyan
            _ => "\x1b[37m",            // White
        };
        let reset = "\x1b[0m";

        writeln!(
            output,
            "{}({}:{}): {}{}{} {}: {}",
            diag.file_name,
            diag.line(),
            diag.column(),
            severity_color,
            diag.category,
            reset,
            diag.ts_code(),
            diag.message
        )
        .ok();

        for related in &diag.related_information {
            writeln!(
                output,
                "  {}({}:{}): {}",
                related.file_name,
                related.line(),
                related.column(),
                related.message
            )
            .ok();
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_diagnostic(category: &str, code: u32) -> Diagnostic {
        Diagnostic {
            file_name: "test.ts".to_string(),
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
            start_pos: 0,
            end_pos: 0,
            code,
            category: category.to_string(),
            message: "test message".to_string(),
            message_chain: vec![],
            related_information: vec![],
            reports_unnecessary: false,
            reports_deprecated: false,
            skipped_on_no_emit: false,
            source_line: String::new(),
        }
    }

    #[test]
    fn test_diagnostic_is_error() {
        let error = make_test_diagnostic("error", 2304);
        assert!(error.is_error());
        assert!(!error.is_warning());

        let warning = make_test_diagnostic("warning", 6133);
        assert!(!warning.is_error());
        assert!(warning.is_warning());
    }

    #[test]
    fn test_has_errors() {
        let no_errors = vec![make_test_diagnostic("warning", 6133)];
        assert!(!has_errors(&no_errors));

        let with_errors = vec![
            make_test_diagnostic("warning", 6133),
            make_test_diagnostic("error", 2304),
        ];
        assert!(has_errors(&with_errors));

        assert!(!has_errors(&[]));
    }

    #[test]
    fn test_error_count() {
        let diagnostics = vec![
            make_test_diagnostic("warning", 6133),
            make_test_diagnostic("error", 2304),
            make_test_diagnostic("error", 2304),
        ];
        assert_eq!(error_count(&diagnostics), 2);
        assert_eq!(warning_count(&diagnostics), 1);
    }

    #[test]
    fn test_format_diagnostics() {
        let mut diag = make_test_diagnostic("error", 2304);
        diag.file_name = "src/main.ts".to_string();
        diag.start = Position {
            line: 9,
            character: 4,
        }; // 0-indexed
        diag.message = "Cannot find name 'foo'".to_string();
        let diagnostics = vec![diag];

        let output = format_diagnostics(&diagnostics);
        assert!(output.contains("src/main.ts(10:5)")); // 1-indexed for display
        assert!(output.contains("error"));
        assert!(output.contains("TS2304"));
        assert!(output.contains("Cannot find name 'foo'"));
    }

    #[test]
    fn test_format_diagnostics_with_related() {
        let mut diag = make_test_diagnostic("error", 2304);
        diag.file_name = "src/main.ts".to_string();
        diag.start = Position {
            line: 9,
            character: 4,
        };
        diag.message = "Cannot find name 'foo'".to_string();

        let mut related = make_test_diagnostic("error", 2304);
        related.file_name = "src/types.ts".to_string();
        related.start = Position {
            line: 4,
            character: 0,
        };
        related.message = "'foo' is declared here".to_string();
        diag.related_information.push(related);

        let output = format_diagnostics(&[diag]);
        assert!(output.contains("src/main.ts(10:5)"));
        assert!(output.contains("src/types.ts(5:1)"));
        assert!(output.contains("'foo' is declared here"));
    }

    #[test]
    fn test_diagnostic_display() {
        let mut diag = make_test_diagnostic("error", 2304);
        diag.file_name = "test.ts".to_string();
        diag.start = Position {
            line: 0,
            character: 4,
        };
        diag.message = "Cannot find name".to_string();
        let display = diag.to_string();
        assert!(display.contains("test.ts(1:5)"));
        assert!(display.contains("error TS2304"));
    }

    #[test]
    fn test_severity_display() {
        assert_eq!(DiagnosticSeverity::Error.to_string(), "error");
        assert_eq!(DiagnosticSeverity::Warning.to_string(), "warning");
        assert_eq!(DiagnosticSeverity::Suggestion.to_string(), "suggestion");
        assert_eq!(DiagnosticSeverity::Message.to_string(), "message");
    }
}
