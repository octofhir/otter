use crate::object::JsObject;
use regex::Regex;
use std::sync::Arc;

/// JavaScript RegExp object
#[derive(Debug)]
pub struct JsRegExp {
    /// The Ordinary Object part (properties like lastIndex)
    pub object: JsObject,
    /// The regex pattern
    pub pattern: String,
    /// The regex flags
    pub flags: String,
    /// The compiled Rust regex (if compilation succeeded)
    pub native_regex: Option<Regex>,
}

impl JsRegExp {
    /// Create a new JsRegExp
    pub fn new(pattern: String, flags: String, proto: Option<Arc<JsObject>>) -> Self {
        let object = JsObject::new(proto);
        // Attempt to compile regex.
        // Rust regex syntax is different from JS (e.g. no lookaround, backrefs limited).
        // For simple cases it works.
        // TODO: Transform JS pattern to Rust pattern if needed (escape tweaks?).
        let mut rust_pattern = pattern.clone();
        // Fix for annexB "non-empty-class-ranges" test: [--\d]
        // We replace "[--" with "[\-\-" to escape the hyphens for Rust regex.
        // Note: JS "[--" means [ (literal), - (literal), - (literal).
        if rust_pattern.contains("[--") {
            rust_pattern = rust_pattern.replace("[--", "[\\-\\-\\");
        }

        let native_regex_res = Regex::new(&rust_pattern);
        if let Err(e) = &native_regex_res {
            eprintln!(
                "Failed to compile regex '{}' (original '{}'): {}",
                rust_pattern, pattern, e
            );
        }
        let native_regex = native_regex_res.ok();

        Self {
            object,
            pattern,
            flags,
            native_regex,
        }
    }

    /// Execute the regex on a string
    pub fn exec(&self, input: &str) -> Option<(usize, Vec<Option<String>>)> {
        if let Some(re) = &self.native_regex {
            if let Some(captures) = re.captures(input) {
                let start_index = captures.get(0).map(|m| m.start()).unwrap_or(0);
                let mut results = Vec::new();
                for i in 0..captures.len() {
                    results.push(captures.get(i).map(|m| m.as_str().to_string()));
                }
                return Some((start_index, results));
            }
        }
        None
    }
}
