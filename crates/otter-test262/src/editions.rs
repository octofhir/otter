//! ECMAScript edition classification for test262 tests
//!
//! Maps test262 feature flags to their introducing ECMAScript edition,
//! enabling per-edition conformance tracking.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::metadata::TestMetadata;

/// ECMAScript specification editions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EsEdition {
    /// ES5 (2009) - the baseline
    ES5,
    /// ES2015 (ES6) - classes, arrows, promises, etc.
    ES2015,
    /// ES2016 (ES7) - Array.includes, exponentiation
    ES2016,
    /// ES2017 (ES8) - async/await, SharedArrayBuffer
    ES2017,
    /// ES2018 (ES9) - rest/spread properties, async iteration
    ES2018,
    /// ES2019 (ES10) - flat, flatMap, Object.fromEntries
    ES2019,
    /// ES2020 (ES11) - BigInt, globalThis, optional chaining
    ES2020,
    /// ES2021 (ES12) - logical assignment, numeric separators
    ES2021,
    /// ES2022 (ES13) - class fields, top-level await
    ES2022,
    /// ES2023 (ES14) - findLast, hashbang, change-array-by-copy
    ES2023,
    /// ES2024 (ES15) - resizable ArrayBuffer, Atomics.waitAsync
    ES2024,
    /// ESNext - proposals not yet standardized
    ESNext,
}

impl std::fmt::Display for EsEdition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EsEdition::ES5 => write!(f, "ES5"),
            EsEdition::ES2015 => write!(f, "ES2015"),
            EsEdition::ES2016 => write!(f, "ES2016"),
            EsEdition::ES2017 => write!(f, "ES2017"),
            EsEdition::ES2018 => write!(f, "ES2018"),
            EsEdition::ES2019 => write!(f, "ES2019"),
            EsEdition::ES2020 => write!(f, "ES2020"),
            EsEdition::ES2021 => write!(f, "ES2021"),
            EsEdition::ES2022 => write!(f, "ES2022"),
            EsEdition::ES2023 => write!(f, "ES2023"),
            EsEdition::ES2024 => write!(f, "ES2024"),
            EsEdition::ESNext => write!(f, "ESNext"),
        }
    }
}

/// Get the edition that introduced a given test262 feature
pub fn feature_edition(feature: &str) -> EsEdition {
    match feature {
        // === ES2015 (ES6) ===
        "arrow-function"
        | "Array.from"
        | "Array.of"
        | "Array.prototype.entries"
        | "Array.prototype.fill"
        | "Array.prototype.find"
        | "Array.prototype.findIndex"
        | "Array.prototype.keys"
        | "Array.prototype.values"
        | "Array.prototype[@@iterator]"
        | "class"
        | "computed-property-names"
        | "const"
        | "DataView"
        | "default-parameters"
        | "destructuring-assignment"
        | "destructuring-binding"
        | "for-of"
        | "generators"
        | "let"
        | "Map"
        | "new.target"
        | "Number.isFinite"
        | "Number.isInteger"
        | "Number.isNaN"
        | "Number.parseFloat"
        | "Number.parseInt"
        | "Number.MAX_SAFE_INTEGER"
        | "Number.MIN_SAFE_INTEGER"
        | "Number.isSafeInteger"
        | "Number.EPSILON"
        | "object-rest"
        | "object-spread"
        | "Object.assign"
        | "Object.getOwnPropertySymbols"
        | "Object.is"
        | "Object.setPrototypeOf"
        | "Promise"
        | "Proxy"
        | "proxy-missing-checks"
        | "Reflect"
        | "Reflect.apply"
        | "Reflect.construct"
        | "Reflect.defineProperty"
        | "Reflect.deleteProperty"
        | "Reflect.get"
        | "Reflect.getOwnPropertyDescriptor"
        | "Reflect.getPrototypeOf"
        | "Reflect.has"
        | "Reflect.isExtensible"
        | "Reflect.ownKeys"
        | "Reflect.preventExtensions"
        | "Reflect.set"
        | "Reflect.setPrototypeOf"
        | "regexp-dotall"
        | "Set"
        | "String.fromCodePoint"
        | "String.prototype.codePointAt"
        | "String.prototype.endsWith"
        | "String.prototype.includes"
        | "String.prototype.normalize"
        | "String.prototype.repeat"
        | "String.prototype.startsWith"
        | "String.prototype[@@iterator]"
        | "String.raw"
        | "super"
        | "Symbol"
        | "Symbol.hasInstance"
        | "Symbol.isConcatSpreadable"
        | "Symbol.iterator"
        | "Symbol.species"
        | "Symbol.toPrimitive"
        | "Symbol.toStringTag"
        | "Symbol.unscopables"
        | "tail-call-optimization"
        | "template"
        | "TypedArray"
        | "u180e"
        | "WeakMap"
        | "WeakSet" => EsEdition::ES2015,

        // === ES2016 (ES7) ===
        "Array.prototype.includes" | "exponentiation" => EsEdition::ES2016,

        // === ES2017 (ES8) ===
        "async-functions"
        | "Atomics"
        | "Object.entries"
        | "Object.getOwnPropertyDescriptors"
        | "Object.values"
        | "SharedArrayBuffer"
        | "String.prototype.padEnd"
        | "String.prototype.padStart" => EsEdition::ES2017,

        // === ES2018 (ES9) ===
        "async-iteration"
        | "dotAll"
        | "named-capture-groups"
        | "regexp-lookbehind"
        | "regexp-named-groups"
        | "regexp-unicode-property-escapes"
        | "s-flag"
        | "Promise.prototype.finally" => EsEdition::ES2018,

        // === ES2019 (ES10) ===
        "Array.prototype.flat"
        | "Array.prototype.flatMap"
        | "json-superset"
        | "Object.fromEntries"
        | "optional-catch-binding"
        | "String.prototype.trimEnd"
        | "String.prototype.trimStart"
        | "Symbol.prototype.description"
        | "well-formed-json-stringify" => EsEdition::ES2019,

        // === ES2020 (ES11) ===
        "BigInt"
        | "dynamic-import"
        | "globalThis"
        | "import.meta"
        | "matchAll"
        | "optional-chaining"
        | "Promise.allSettled"
        | "String.prototype.matchAll"
        | "nullish-coalescing" => EsEdition::ES2020,

        // === ES2021 (ES12) ===
        "AggregateError"
        | "FinalizationRegistry"
        | "logical-assignment-operators"
        | "numeric-separator-literal"
        | "Promise.any"
        | "String.prototype.replaceAll"
        | "WeakRef" => EsEdition::ES2021,

        // === ES2022 (ES13) ===
        "class-fields-private"
        | "class-fields-public"
        | "class-methods-private"
        | "class-static-block"
        | "class-static-fields-private"
        | "class-static-fields-public"
        | "class-static-methods-private"
        | "error-cause"
        | "Object.hasOwn"
        | "regexp-match-indices"
        | "top-level-await"
        | "at"
        | "Array.prototype.at"
        | "String.prototype.at"
        | "TypedArray.prototype.at" => EsEdition::ES2022,

        // === ES2023 (ES14) ===
        "Array.prototype.findLast"
        | "Array.prototype.findLastIndex"
        | "change-array-by-copy"
        | "hashbang"
        | "symbols-as-weakmap-keys"
        | "Array.prototype.toReversed"
        | "Array.prototype.toSorted"
        | "Array.prototype.toSpliced"
        | "Array.prototype.with"
        | "TypedArray.prototype.findLast"
        | "TypedArray.prototype.findLastIndex"
        | "TypedArray.prototype.toReversed"
        | "TypedArray.prototype.toSorted"
        | "TypedArray.prototype.with" => EsEdition::ES2023,

        // === ES2024 (ES15) ===
        "arraybuffer-transfer"
        | "resizable-arraybuffer"
        | "Atomics.waitAsync"
        | "promise-with-resolvers"
        | "regexp-v-flag"
        | "array-grouping"
        | "Object.groupBy"
        | "Map.groupBy"
        | "well-formed-unicode-strings"
        | "String.prototype.isWellFormed"
        | "String.prototype.toWellFormed" => EsEdition::ES2024,

        // === ESNext (proposals) ===
        "Temporal"
        | "decorators"
        | "explicit-resource-management"
        | "import-assertions"
        | "import-attributes"
        | "json-modules"
        | "ShadowRealm"
        | "iterator-helpers"
        | "set-methods"
        | "Array.fromAsync"
        | "Uint8Array"
        | "regexp-duplicate-named-groups"
        | "Intl"
        | "Intl.DateTimeFormat"
        | "Intl.DisplayNames"
        | "Intl.ListFormat"
        | "Intl.Locale"
        | "Intl.NumberFormat"
        | "Intl.PluralRules"
        | "Intl.RelativeTimeFormat"
        | "Intl.Segmenter"
        | "Intl-enumeration" => EsEdition::ESNext,

        // Default: if no feature tag, it's a basic ES5 test
        _ => EsEdition::ES5,
    }
}

/// Classify a test by its highest-required ECMAScript edition.
pub fn classify_test(metadata: &TestMetadata) -> EsEdition {
    if metadata.features.is_empty() {
        return EsEdition::ES5;
    }

    metadata
        .features
        .iter()
        .map(|f| feature_edition(f))
        .max()
        .unwrap_or(EsEdition::ES5)
}

/// Per-edition conformance report
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EditionReport {
    /// Total tests for this edition
    pub total: usize,
    /// Passed tests
    pub passed: usize,
    /// Failed tests
    pub failed: usize,
    /// Skipped tests
    pub skipped: usize,
}

impl EditionReport {
    /// Pass rate as percentage (excluding skipped)
    pub fn pass_rate(&self) -> f64 {
        let run = self.passed + self.failed;
        if run > 0 {
            (self.passed as f64 / run as f64) * 100.0
        } else {
            0.0
        }
    }
}

/// Print edition statistics table
pub fn print_edition_table(editions: &HashMap<EsEdition, EditionReport>) {
    use colored::*;

    println!();
    println!(
        "{}",
        "=== Conformance by ECMAScript Edition ===".bold().cyan()
    );
    println!(
        "{:<10} {:>8} {:>8} {:>8} {:>8} {:>10}",
        "Edition", "Total", "Pass", "Fail", "Skip", "Pass %"
    );
    println!("{}", "-".repeat(58));

    let mut editions_sorted: Vec<_> = editions.iter().collect();
    editions_sorted.sort_by_key(|(k, _)| **k);

    for (edition, report) in editions_sorted {
        let rate = report.pass_rate();
        let rate_str = if rate >= 90.0 {
            format!("{:.1}%", rate).green()
        } else if rate >= 50.0 {
            format!("{:.1}%", rate).yellow()
        } else {
            format!("{:.1}%", rate).red()
        };

        println!(
            "{:<10} {:>8} {:>8} {:>8} {:>8} {:>10}",
            edition.to_string(),
            report.total,
            report.passed,
            report.failed,
            report.skipped,
            rate_str,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature_edition_mapping() {
        assert_eq!(feature_edition("arrow-function"), EsEdition::ES2015);
        assert_eq!(feature_edition("async-functions"), EsEdition::ES2017);
        assert_eq!(feature_edition("BigInt"), EsEdition::ES2020);
        assert_eq!(feature_edition("class-fields-public"), EsEdition::ES2022);
        assert_eq!(feature_edition("Temporal"), EsEdition::ESNext);
        assert_eq!(feature_edition("unknown-feature"), EsEdition::ES5);
    }

    #[test]
    fn test_classify_test() {
        let mut metadata = TestMetadata::default();
        assert_eq!(classify_test(&metadata), EsEdition::ES5);

        metadata.features = vec!["arrow-function".to_string(), "BigInt".to_string()];
        assert_eq!(classify_test(&metadata), EsEdition::ES2020);
    }
}
