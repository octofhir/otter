//! Test262 test metadata parsing

use serde::{Deserialize, Serialize};

/// Test262 test metadata (from YAML frontmatter)
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TestMetadata {
    /// Test description
    #[serde(default)]
    pub description: String,

    /// ES version info
    #[serde(default)]
    pub esid: Option<String>,

    /// Test information
    #[serde(default)]
    pub info: Option<String>,

    /// Features required by this test
    #[serde(default)]
    pub features: Vec<String>,

    /// Test flags
    #[serde(default)]
    pub flags: Vec<String>,

    /// Negative test expectation
    #[serde(default)]
    pub negative: Option<NegativeExpectation>,

    /// Includes (harness files)
    #[serde(default)]
    pub includes: Vec<String>,

    /// Locale for Intl tests
    #[serde(default)]
    pub locale: Vec<String>,
}

/// Negative test expectation
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NegativeExpectation {
    /// Phase when error should occur
    pub phase: ErrorPhase,
    /// Expected error type
    #[serde(rename = "type")]
    pub error_type: String,
}

/// Execution mode for a test
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum ExecutionMode {
    /// Run in strict mode ("use strict" prepended)
    Strict,
    /// Run in non-strict (sloppy) mode
    NonStrict,
    /// Run as an ES module
    Module,
}

impl std::fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutionMode::Strict => write!(f, "strict"),
            ExecutionMode::NonStrict => write!(f, "non-strict"),
            ExecutionMode::Module => write!(f, "module"),
        }
    }
}

/// Phase when an error is expected
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorPhase {
    /// Parse-time error
    Parse,
    /// Early error (static semantics)
    Early,
    /// Resolution error (module linking)
    Resolution,
    /// Runtime error
    Runtime,
}

impl TestMetadata {
    /// Parse metadata from test file content
    pub fn parse(content: &str) -> Option<Self> {
        // Find YAML frontmatter between /*--- and ---*/
        let start = content.find("/*---")?;
        let end = content.find("---*/")?;

        if start >= end {
            return None;
        }

        let yaml_content = &content[start + 5..end];
        serde_yaml::from_str(yaml_content).ok()
    }

    /// Check if test should run in strict mode
    pub fn is_strict(&self) -> bool {
        self.flags.contains(&"onlyStrict".to_string())
    }

    /// Check if test should run in non-strict mode
    pub fn is_nostrict(&self) -> bool {
        self.flags.contains(&"noStrict".to_string())
    }

    /// Check if this is a module test
    pub fn is_module(&self) -> bool {
        self.flags.contains(&"module".to_string())
    }

    /// Check if this is an async test
    pub fn is_async(&self) -> bool {
        self.flags.contains(&"async".to_string())
    }

    /// Check if test expects a parse/early error
    pub fn expects_early_error(&self) -> bool {
        matches!(
            &self.negative,
            Some(NegativeExpectation {
                phase: ErrorPhase::Parse | ErrorPhase::Early,
                ..
            })
        )
    }

    /// Get the execution modes this test should run in.
    ///
    /// Per the test262 spec:
    /// - `onlyStrict` flag → strict mode only
    /// - `noStrict` flag → non-strict mode only
    /// - `module` flag → module mode only (skipped for now)
    /// - no flag → both strict and non-strict
    pub fn execution_modes(&self) -> Vec<ExecutionMode> {
        if self.is_module() {
            return vec![ExecutionMode::Module];
        }
        if self.is_strict() {
            return vec![ExecutionMode::Strict];
        }
        if self.is_nostrict() {
            return vec![ExecutionMode::NonStrict];
        }
        // Default: run in both modes
        vec![ExecutionMode::NonStrict, ExecutionMode::Strict]
    }

    /// Check if test expects a runtime error
    pub fn expects_runtime_error(&self) -> bool {
        matches!(
            &self.negative,
            Some(NegativeExpectation {
                phase: ErrorPhase::Runtime,
                ..
            })
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_metadata() {
        let content = r#"
/*---
description: Test addition
features: [BigInt]
flags: [onlyStrict]
---*/
1 + 1;
"#;

        let meta = TestMetadata::parse(content).unwrap();
        assert_eq!(meta.description, "Test addition");
        assert!(meta.features.contains(&"BigInt".to_string()));
        assert!(meta.is_strict());
    }

    #[test]
    fn test_negative_expectation() {
        let content = r#"
/*---
description: Test syntax error
negative:
  phase: parse
  type: SyntaxError
---*/
{{{
"#;

        let meta = TestMetadata::parse(content).unwrap();
        assert!(meta.expects_early_error());
        assert_eq!(meta.negative.as_ref().unwrap().error_type, "SyntaxError");
    }
}
