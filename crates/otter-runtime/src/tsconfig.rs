//! tsconfig.json parsing and handling.
//!
//! This module provides functionality to read, parse, and apply TypeScript
//! configuration from tsconfig.json files.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use swc_ecma_ast::EsVersion;

use crate::config::TypeScriptConfig;
use crate::error::{JscError, JscResult};

/// Parsed tsconfig.json file structure.
///
/// This struct represents the full tsconfig.json schema, though only
/// a subset of options are used for transpilation.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsConfigJson {
    /// Compiler options
    #[serde(default)]
    pub compiler_options: CompilerOptions,

    /// Files to include
    #[serde(default)]
    pub include: Vec<String>,

    /// Files to exclude
    #[serde(default)]
    pub exclude: Vec<String>,

    /// Path to base tsconfig to extend from
    #[serde(default)]
    pub extends: Option<String>,

    /// File references for project references
    #[serde(default)]
    pub references: Vec<ProjectReference>,
}

/// TypeScript compiler options from tsconfig.json.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompilerOptions {
    /// ECMAScript target version (e.g., "ES2020", "ES2022", "ESNext")
    #[serde(default)]
    pub target: Option<String>,

    /// Module system (e.g., "commonjs", "esnext", "nodenext")
    #[serde(default)]
    pub module: Option<String>,

    /// Module resolution strategy
    #[serde(default)]
    pub module_resolution: Option<String>,

    /// JSX emit mode (e.g., "react", "react-jsx", "preserve")
    #[serde(default)]
    pub jsx: Option<String>,

    /// Enable strict mode (all strict type checking options)
    #[serde(default)]
    pub strict: Option<bool>,

    /// Allow JavaScript files
    #[serde(default)]
    pub allow_js: Option<bool>,

    /// Check JavaScript files
    #[serde(default)]
    pub check_js: Option<bool>,

    /// Enable experimental decorators
    #[serde(default)]
    pub experimental_decorators: Option<bool>,

    /// Emit decorator metadata
    #[serde(default)]
    pub emit_decorator_metadata: Option<bool>,

    /// Generate source maps
    #[serde(default)]
    pub source_map: Option<bool>,

    /// Inline source maps in output
    #[serde(default)]
    pub inline_source_map: Option<bool>,

    /// Include source content in source maps
    #[serde(default)]
    pub inline_sources: Option<bool>,

    /// Output directory for compiled files
    #[serde(default)]
    pub out_dir: Option<String>,

    /// Root directory of source files
    #[serde(default)]
    pub root_dir: Option<String>,

    /// Base URL for module resolution
    #[serde(default)]
    pub base_url: Option<String>,

    /// Path mapping for module resolution
    #[serde(default)]
    pub paths: Option<HashMap<String, Vec<String>>>,

    /// Skip type checking of declaration files
    #[serde(default)]
    pub skip_lib_check: Option<bool>,

    /// ES module interop
    #[serde(default)]
    pub es_module_interop: Option<bool>,

    /// Allow synthetic default imports
    #[serde(default)]
    pub allow_synthetic_default_imports: Option<bool>,

    /// Resolve JSON modules
    #[serde(default)]
    pub resolve_json_module: Option<bool>,

    /// Isolated modules (each file is a separate module)
    #[serde(default)]
    pub isolated_modules: Option<bool>,

    /// No emit (type check only)
    #[serde(default)]
    pub no_emit: Option<bool>,

    /// Declaration file generation
    #[serde(default)]
    pub declaration: Option<bool>,

    /// Declaration map generation
    #[serde(default)]
    pub declaration_map: Option<bool>,

    /// Type roots for @types packages
    #[serde(default)]
    pub type_roots: Option<Vec<String>>,

    /// Types to include
    #[serde(default)]
    pub types: Option<Vec<String>>,

    /// Lib files to include (e.g., "ES2020", "DOM")
    #[serde(default)]
    pub lib: Option<Vec<String>>,

    // Strict mode sub-options
    #[serde(default)]
    pub no_implicit_any: Option<bool>,
    #[serde(default)]
    pub strict_null_checks: Option<bool>,
    #[serde(default)]
    pub strict_function_types: Option<bool>,
    #[serde(default)]
    pub strict_bind_call_apply: Option<bool>,
    #[serde(default)]
    pub strict_property_initialization: Option<bool>,
    #[serde(default)]
    pub no_implicit_this: Option<bool>,
    #[serde(default)]
    pub always_strict: Option<bool>,
    #[serde(default)]
    pub use_unknown_in_catch_variables: Option<bool>,
}

/// Project reference in tsconfig.json
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectReference {
    pub path: String,
}

impl TsConfigJson {
    /// Load tsconfig.json from a file path.
    pub fn load(path: impl AsRef<Path>) -> JscResult<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|e| {
            JscError::internal(format!("Failed to read tsconfig.json at {:?}: {}", path, e))
        })?;

        Self::parse(&content)
    }

    /// Parse tsconfig.json from a string.
    ///
    /// Note: This uses a JSON5-compatible parser that handles trailing commas
    /// and comments (which are common in tsconfig.json files).
    pub fn parse(content: &str) -> JscResult<Self> {
        // Strip comments and trailing commas for standard JSON parsing
        let cleaned = strip_json_comments(content);

        serde_json::from_str(&cleaned)
            .map_err(|e| JscError::internal(format!("Failed to parse tsconfig.json: {}", e)))
    }

    /// Load tsconfig.json with extends resolution.
    ///
    /// This recursively loads any extended configurations and merges them.
    pub fn load_with_extends(path: impl AsRef<Path>) -> JscResult<Self> {
        let path = path.as_ref();
        let mut config = Self::load(path)?;

        if let Some(extends) = config.extends.take() {
            let base_dir = path.parent().unwrap_or(Path::new("."));
            let extends_path = resolve_extends_path(base_dir, &extends)?;
            let base_config = Self::load_with_extends(&extends_path)?;
            config = merge_configs(base_config, config);
        }

        Ok(config)
    }

    /// Convert to TypeScriptConfig for use with the transpiler.
    pub fn to_typescript_config(&self) -> TypeScriptConfig {
        let mut config = TypeScriptConfig::default();

        // Apply target
        if let Some(target) = &self.compiler_options.target {
            config.target = parse_es_target(target);
        }

        // Apply strict mode
        if let Some(strict) = self.compiler_options.strict {
            config.strict = strict;
        }

        // Apply skip lib check
        if let Some(skip) = self.compiler_options.skip_lib_check {
            config.skip_lib_check = skip;
        }

        // Apply source maps
        if self.compiler_options.source_map == Some(true)
            || self.compiler_options.inline_source_map == Some(true)
        {
            config.source_maps = true;
        }

        // Apply decorators
        if let Some(decorators) = self.compiler_options.experimental_decorators {
            config.decorators = decorators;
        }

        // Apply JSX/TSX
        if let Some(jsx) = &self.compiler_options.jsx {
            config.tsx = jsx != "none";
        }

        config
    }
}

/// Find tsconfig.json by walking up from a starting directory.
///
/// Returns the path to tsconfig.json if found, or None if not found.
pub fn find_tsconfig(start_dir: impl AsRef<Path>) -> Option<PathBuf> {
    let mut current = start_dir.as_ref().to_path_buf();

    loop {
        let tsconfig_path = current.join("tsconfig.json");
        if tsconfig_path.exists() {
            return Some(tsconfig_path);
        }

        if !current.pop() {
            return None;
        }
    }
}

/// Find and load tsconfig.json from a directory.
///
/// Searches for tsconfig.json starting from `start_dir` and walking up
/// the directory tree. If found, loads and parses it (including extends).
pub fn load_tsconfig_for_dir(start_dir: impl AsRef<Path>) -> JscResult<Option<TsConfigJson>> {
    match find_tsconfig(start_dir) {
        Some(path) => Ok(Some(TsConfigJson::load_with_extends(path)?)),
        None => Ok(None),
    }
}

/// Find and load tsconfig.json, returning a TypeScriptConfig.
///
/// This is a convenience function that finds tsconfig.json, parses it,
/// and converts it to a TypeScriptConfig ready for use.
pub fn load_typescript_config_for_dir(start_dir: impl AsRef<Path>) -> JscResult<TypeScriptConfig> {
    match load_tsconfig_for_dir(start_dir)? {
        Some(tsconfig) => Ok(tsconfig.to_typescript_config()),
        None => Ok(TypeScriptConfig::default()),
    }
}

/// Parse an ECMAScript target string to EsVersion.
fn parse_es_target(target: &str) -> EsVersion {
    match target.to_uppercase().as_str() {
        "ES3" => EsVersion::Es3,
        "ES5" => EsVersion::Es5,
        "ES2015" | "ES6" => EsVersion::Es2015,
        "ES2016" => EsVersion::Es2016,
        "ES2017" => EsVersion::Es2017,
        "ES2018" => EsVersion::Es2018,
        "ES2019" => EsVersion::Es2019,
        "ES2020" => EsVersion::Es2020,
        "ES2021" => EsVersion::Es2021,
        "ES2022" => EsVersion::Es2022,
        "ESNEXT" | "ES2023" | "ES2024" => EsVersion::EsNext,
        _ => EsVersion::Es2022, // Default
    }
}

/// Resolve the path from an "extends" field.
fn resolve_extends_path(base_dir: &Path, extends: &str) -> JscResult<PathBuf> {
    if extends.starts_with('.') {
        // Relative path
        let mut path = base_dir.join(extends);
        if !path.exists() && !extends.ends_with(".json") {
            path = base_dir.join(format!("{}.json", extends));
        }
        Ok(path)
    } else if extends.starts_with('@') || !extends.contains('/') {
        // Node module (e.g., "@tsconfig/node20/tsconfig.json")
        // Try to resolve from node_modules
        let node_modules = base_dir.join("node_modules").join(extends);
        if node_modules.exists() {
            return Ok(node_modules);
        }

        // Try with tsconfig.json appended
        let with_tsconfig = base_dir
            .join("node_modules")
            .join(extends)
            .join("tsconfig.json");
        if with_tsconfig.exists() {
            return Ok(with_tsconfig);
        }

        Err(JscError::internal(format!(
            "Could not resolve extends: {}",
            extends
        )))
    } else {
        Ok(base_dir.join(extends))
    }
}

/// Merge two TsConfigJson structs (base is overridden by overlay).
fn merge_configs(base: TsConfigJson, overlay: TsConfigJson) -> TsConfigJson {
    TsConfigJson {
        compiler_options: merge_compiler_options(base.compiler_options, overlay.compiler_options),
        include: if overlay.include.is_empty() {
            base.include
        } else {
            overlay.include
        },
        exclude: if overlay.exclude.is_empty() {
            base.exclude
        } else {
            overlay.exclude
        },
        extends: None, // Already resolved
        references: if overlay.references.is_empty() {
            base.references
        } else {
            overlay.references
        },
    }
}

/// Merge compiler options (overlay takes precedence).
fn merge_compiler_options(base: CompilerOptions, overlay: CompilerOptions) -> CompilerOptions {
    CompilerOptions {
        target: overlay.target.or(base.target),
        module: overlay.module.or(base.module),
        module_resolution: overlay.module_resolution.or(base.module_resolution),
        jsx: overlay.jsx.or(base.jsx),
        strict: overlay.strict.or(base.strict),
        allow_js: overlay.allow_js.or(base.allow_js),
        check_js: overlay.check_js.or(base.check_js),
        experimental_decorators: overlay
            .experimental_decorators
            .or(base.experimental_decorators),
        emit_decorator_metadata: overlay
            .emit_decorator_metadata
            .or(base.emit_decorator_metadata),
        source_map: overlay.source_map.or(base.source_map),
        inline_source_map: overlay.inline_source_map.or(base.inline_source_map),
        inline_sources: overlay.inline_sources.or(base.inline_sources),
        out_dir: overlay.out_dir.or(base.out_dir),
        root_dir: overlay.root_dir.or(base.root_dir),
        base_url: overlay.base_url.or(base.base_url),
        paths: overlay.paths.or(base.paths),
        skip_lib_check: overlay.skip_lib_check.or(base.skip_lib_check),
        es_module_interop: overlay.es_module_interop.or(base.es_module_interop),
        allow_synthetic_default_imports: overlay
            .allow_synthetic_default_imports
            .or(base.allow_synthetic_default_imports),
        resolve_json_module: overlay.resolve_json_module.or(base.resolve_json_module),
        isolated_modules: overlay.isolated_modules.or(base.isolated_modules),
        no_emit: overlay.no_emit.or(base.no_emit),
        declaration: overlay.declaration.or(base.declaration),
        declaration_map: overlay.declaration_map.or(base.declaration_map),
        type_roots: overlay.type_roots.or(base.type_roots),
        types: overlay.types.or(base.types),
        lib: overlay.lib.or(base.lib),
        no_implicit_any: overlay.no_implicit_any.or(base.no_implicit_any),
        strict_null_checks: overlay.strict_null_checks.or(base.strict_null_checks),
        strict_function_types: overlay.strict_function_types.or(base.strict_function_types),
        strict_bind_call_apply: overlay
            .strict_bind_call_apply
            .or(base.strict_bind_call_apply),
        strict_property_initialization: overlay
            .strict_property_initialization
            .or(base.strict_property_initialization),
        no_implicit_this: overlay.no_implicit_this.or(base.no_implicit_this),
        always_strict: overlay.always_strict.or(base.always_strict),
        use_unknown_in_catch_variables: overlay
            .use_unknown_in_catch_variables
            .or(base.use_unknown_in_catch_variables),
    }
}

/// Strip single-line and multi-line comments from JSON.
///
/// tsconfig.json files commonly contain comments, which standard JSON
/// parsers don't support. This function removes them.
fn strip_json_comments(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape_next = false;

    while let Some(c) = chars.next() {
        if escape_next {
            result.push(c);
            escape_next = false;
            continue;
        }

        if c == '\\' && in_string {
            result.push(c);
            escape_next = true;
            continue;
        }

        if c == '"' && !escape_next {
            in_string = !in_string;
            result.push(c);
            continue;
        }

        if in_string {
            result.push(c);
            continue;
        }

        // Check for comments
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    // Single-line comment - skip until newline
                    chars.next();
                    for nc in chars.by_ref() {
                        if nc == '\n' {
                            result.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    // Multi-line comment - skip until */
                    chars.next();
                    while let Some(nc) = chars.next() {
                        if nc == '*'
                            && chars.peek() == Some(&'/') {
                                chars.next();
                                break;
                            }
                    }
                }
                _ => result.push(c),
            }
        } else {
            result.push(c);
        }
    }

    // Also handle trailing commas (common in tsconfig.json)
    strip_trailing_commas(&result)
}

/// Strip trailing commas from JSON arrays and objects.
fn strip_trailing_commas(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape_next = false;

    while let Some(c) = chars.next() {
        if escape_next {
            result.push(c);
            escape_next = false;
            continue;
        }

        if c == '\\' && in_string {
            result.push(c);
            escape_next = true;
            continue;
        }

        if c == '"' && !escape_next {
            in_string = !in_string;
            result.push(c);
            continue;
        }

        if in_string {
            result.push(c);
            continue;
        }

        if c == ',' {
            // Look ahead for ] or } (skipping whitespace)
            let temp_chars = chars.clone();
            let mut is_trailing = false;

            for nc in temp_chars {
                if nc.is_whitespace() {
                    continue;
                }
                if nc == ']' || nc == '}' {
                    is_trailing = true;
                }
                break;
            }

            if !is_trailing {
                result.push(c);
            }
        } else {
            result.push(c);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_tsconfig() {
        let json = r#"
        {
            "compilerOptions": {
                "target": "ES2022",
                "module": "esnext",
                "strict": true
            }
        }
        "#;

        let config = TsConfigJson::parse(json).unwrap();
        assert_eq!(config.compiler_options.target, Some("ES2022".to_string()));
        assert_eq!(config.compiler_options.module, Some("esnext".to_string()));
        assert_eq!(config.compiler_options.strict, Some(true));
    }

    #[test]
    fn test_parse_with_comments() {
        let json = r#"
        {
            // This is a comment
            "compilerOptions": {
                "target": "ES2020", // inline comment
                /* multi-line
                   comment */
                "strict": true
            }
        }
        "#;

        let config = TsConfigJson::parse(json).unwrap();
        assert_eq!(config.compiler_options.target, Some("ES2020".to_string()));
        assert_eq!(config.compiler_options.strict, Some(true));
    }

    #[test]
    fn test_parse_with_trailing_commas() {
        let json = r#"
        {
            "compilerOptions": {
                "target": "ES2020",
                "strict": true,
            },
        }
        "#;

        let config = TsConfigJson::parse(json).unwrap();
        assert_eq!(config.compiler_options.target, Some("ES2020".to_string()));
    }

    #[test]
    fn test_convert_to_typescript_config() {
        let json = r#"
        {
            "compilerOptions": {
                "target": "ES2020",
                "strict": false,
                "sourceMap": true,
                "experimentalDecorators": true
            }
        }
        "#;

        let tsconfig = TsConfigJson::parse(json).unwrap();
        let config = tsconfig.to_typescript_config();

        assert_eq!(config.target, EsVersion::Es2020);
        assert!(!config.strict);
        assert!(config.source_maps);
        assert!(config.decorators);
    }

    #[test]
    fn test_parse_es_target() {
        assert_eq!(parse_es_target("ES5"), EsVersion::Es5);
        assert_eq!(parse_es_target("ES2015"), EsVersion::Es2015);
        assert_eq!(parse_es_target("ES6"), EsVersion::Es2015);
        assert_eq!(parse_es_target("ES2020"), EsVersion::Es2020);
        assert_eq!(parse_es_target("ES2022"), EsVersion::Es2022);
        assert_eq!(parse_es_target("ESNext"), EsVersion::EsNext);
        assert_eq!(parse_es_target("esnext"), EsVersion::EsNext);
    }

    #[test]
    fn test_strip_json_comments() {
        let input = r#"{"key": "value" /* comment */}"#;
        let result = strip_json_comments(input);
        assert_eq!(result.trim(), r#"{"key": "value" }"#);

        let input2 = r#"{"key": "value"} // comment"#;
        let result2 = strip_json_comments(input2);
        assert!(result2.contains(r#"{"key": "value"}"#));
    }

    #[test]
    fn test_strip_trailing_commas() {
        let input = r#"{"key": "value",}"#;
        let result = strip_trailing_commas(input);
        assert_eq!(result, r#"{"key": "value"}"#);

        let input2 = r#"["a", "b",]"#;
        let result2 = strip_trailing_commas(input2);
        assert_eq!(result2, r#"["a", "b"]"#);
    }

    #[test]
    fn test_complex_tsconfig() {
        let json = r#"
        {
            "compilerOptions": {
                "target": "ES2022",
                "module": "NodeNext",
                "moduleResolution": "NodeNext",
                "strict": true,
                "esModuleInterop": true,
                "skipLibCheck": true,
                "forceConsistentCasingInFileNames": true,
                "outDir": "./dist",
                "rootDir": "./src",
                "declaration": true,
                "declarationMap": true,
                "sourceMap": true,
                "paths": {
                    "@/*": ["./src/*"]
                }
            },
            "include": ["src/**/*"],
            "exclude": ["node_modules", "dist"]
        }
        "#;

        let config = TsConfigJson::parse(json).unwrap();
        assert_eq!(config.compiler_options.target, Some("ES2022".to_string()));
        assert_eq!(config.compiler_options.module, Some("NodeNext".to_string()));
        assert!(config.compiler_options.paths.is_some());
        assert_eq!(config.include, vec!["src/**/*"]);
        assert_eq!(config.exclude, vec!["node_modules", "dist"]);
    }

    #[test]
    fn test_merge_configs() {
        let base = TsConfigJson {
            compiler_options: CompilerOptions {
                target: Some("ES2020".to_string()),
                strict: Some(true),
                source_map: Some(false),
                ..Default::default()
            },
            ..Default::default()
        };

        let overlay = TsConfigJson {
            compiler_options: CompilerOptions {
                target: Some("ES2022".to_string()),
                source_map: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };

        let merged = merge_configs(base, overlay);
        assert_eq!(merged.compiler_options.target, Some("ES2022".to_string()));
        assert_eq!(merged.compiler_options.strict, Some(true)); // from base
        assert_eq!(merged.compiler_options.source_map, Some(true)); // from overlay
    }
}
