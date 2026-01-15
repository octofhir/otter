//! Embedded TypeScript type definitions.
//!
//! This module provides access to built-in TypeScript type definitions
//! that are embedded in the binary at compile time.

// Include the generated code from build.rs
include!(concat!(env!("OUT_DIR"), "/embedded_types.rs"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_types_exist() {
        // Should have at least otter.d.ts
        assert!(!EMBEDDED_TYPES.is_empty(), "No embedded types found");
    }

    #[test]
    fn test_get_otter_types() {
        let otter_types = get_embedded_type("otter.d.ts");
        assert!(otter_types.is_some(), "otter.d.ts not found");

        let contents = otter_types.unwrap();
        assert!(contents.contains("console"), "Should contain console API");
        assert!(contents.contains("setTimeout"), "Should contain timer APIs");
    }

    #[test]
    fn test_list_embedded_types() {
        let types: Vec<_> = list_embedded_types().collect();
        assert!(!types.is_empty());
        assert!(types.iter().any(|t| t.contains("otter.d.ts")));
    }
}
