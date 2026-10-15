//! WHATWG import maps and module specifier resolution for browser hosts.
//!
//! A page's `<script type="importmap">` rewrites the specifiers its modules
//! request before any of them is fetched. This module implements the HTML
//! Standard's algorithms over owned data: parsing and normalizing an import
//! map, merging a later map into the one already registered, and resolving a
//! module specifier against a base URL through it.
//!
//! # Contents
//! - [`ImportMap`] — a parsed, normalized import map (top-level `imports` and
//!   `scopes`), with [`ImportMap::parse`], [`ImportMap::merge`] and
//!   [`ImportMap::resolve`].
//! - [`ImportMapResolveError`] — the `TypeError` a failed resolution throws.
//!
//! # Invariants
//! - Specifier maps hold their keys in descending code-unit order, so the
//!   first matching prefix is the longest one, as the standard requires.
//! - An address that failed to parse or normalize is kept as `None`, which
//!   blocks the key instead of letting it fall through to a less specific
//!   entry.
//! - Merging never overrides a rule already registered: the first map that
//!   defines a key or a scoped key wins.
//!
//! # See also
//! - <https://html.spec.whatwg.org/multipage/webappapis.html#import-maps>
//! - <https://html.spec.whatwg.org/multipage/webappapis.html#resolve-a-module-specifier>
//! - [`crate::web_modules`] — the module map that consults this resolver.

use url::Url;

/// One normalized specifier map: `(key, address)` sorted by key, descending.
type SpecifierMap = Vec<(String, Option<Url>)>;

/// A parsed and normalized import map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportMap {
    imports: SpecifierMap,
    /// `(scope prefix, specifier map)`, sorted by prefix, descending.
    scopes: Vec<(String, SpecifierMap)>,
}

/// Why a module specifier did not resolve. Surfaces to script as a
/// `TypeError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ImportMapResolveError {
    /// Human-readable reason, naming the specifier.
    pub message: String,
}

impl ImportMap {
    /// Parse an import map's JSON text against the URL of the document that
    /// holds it, returning the map and the warnings the standard says to
    /// report for entries it ignores.
    ///
    /// # Errors
    /// A string that is not JSON, or JSON whose top level, `imports` or
    /// `scopes` is not an object. The page sees these as a `TypeError` (or a
    /// `SyntaxError` for invalid JSON) reported to its global.
    pub fn parse(text: &str, base_url: &str) -> Result<(Self, Vec<String>), String> {
        let base = Url::parse(base_url).map_err(|error| format!("invalid base URL: {error}"))?;
        let parsed: serde_json::Value = serde_json::from_str(text)
            .map_err(|error| format!("invalid import map JSON: {error}"))?;
        let serde_json::Value::Object(top) = parsed else {
            return Err("an import map must be a JSON object".to_owned());
        };
        let mut warnings = Vec::new();
        let imports = match top.get("imports") {
            None => Vec::new(),
            Some(serde_json::Value::Object(map)) => {
                normalize_specifier_map(map, &base, &mut warnings)
            }
            Some(_) => return Err("an import map's \"imports\" must be an object".to_owned()),
        };
        let scopes = match top.get("scopes") {
            None => Vec::new(),
            Some(serde_json::Value::Object(scopes)) => {
                let mut normalized = Vec::new();
                for (prefix, map) in scopes {
                    let serde_json::Value::Object(map) = map else {
                        return Err(format!(
                            "the import map scope \"{prefix}\" must map to an object"
                        ));
                    };
                    let Ok(prefix_url) = base.join(prefix) else {
                        warnings.push(format!("ignored scope \"{prefix}\": not a URL"));
                        continue;
                    };
                    normalized.push((
                        prefix_url.to_string(),
                        normalize_specifier_map(map, &base, &mut warnings),
                    ));
                }
                normalized.sort_by(|a, b| b.0.cmp(&a.0));
                normalized
            }
            Some(_) => return Err("an import map's \"scopes\" must be an object".to_owned()),
        };
        for key in top.keys() {
            if !matches!(key.as_str(), "imports" | "scopes" | "integrity") {
                warnings.push(format!("ignored unknown import map key \"{key}\""));
            }
        }
        Ok((Self { imports, scopes }, warnings))
    }

    /// Whether the map has no rules at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.imports.is_empty() && self.scopes.is_empty()
    }

    /// Merge a later import map into this one. Rules this map already has
    /// win; the new map only adds what is missing.
    pub fn merge(&mut self, newer: Self) {
        for (key, address) in newer.imports {
            if !self.imports.iter().any(|(existing, _)| *existing == key) {
                self.imports.push((key, address));
            }
        }
        self.imports.sort_by(|a, b| b.0.cmp(&a.0));
        for (prefix, map) in newer.scopes {
            match self
                .scopes
                .iter_mut()
                .find(|(existing, _)| *existing == prefix)
            {
                Some((_, existing)) => {
                    for (key, address) in map {
                        if !existing.iter().any(|(have, _)| *have == key) {
                            existing.push((key, address));
                        }
                    }
                    existing.sort_by(|a, b| b.0.cmp(&a.0));
                }
                None => self.scopes.push((prefix, map)),
            }
        }
        self.scopes.sort_by(|a, b| b.0.cmp(&a.0));
    }

    /// Resolve `specifier` as requested by a module whose base URL is `base`.
    ///
    /// # Errors
    /// A bare specifier the map does not cover, a specifier the map blocks,
    /// or one whose mapped address does not parse.
    pub fn resolve(&self, specifier: &str, base: &Url) -> Result<Url, ImportMapResolveError> {
        let as_url = parse_url_like_specifier(specifier, base);
        let normalized = as_url
            .as_ref()
            .map_or_else(|| specifier.to_owned(), Url::to_string);
        let base_string = base.as_str();
        for (prefix, map) in &self.scopes {
            let applies = prefix == base_string
                || (prefix.ends_with('/') && base_string.starts_with(prefix.as_str()));
            if applies
                && let Some(result) = resolve_imports_match(&normalized, as_url.as_ref(), map)
            {
                return result;
            }
        }
        if let Some(result) = resolve_imports_match(&normalized, as_url.as_ref(), &self.imports) {
            return result;
        }
        as_url.ok_or_else(|| ImportMapResolveError {
            message: format!(
                "Failed to resolve module specifier \"{specifier}\". Relative references must \
                 start with either \"/\", \"./\", or \"../\"."
            ),
        })
    }
}

/// HTML's "resolve a URL-like module specifier".
fn parse_url_like_specifier(specifier: &str, base: &Url) -> Option<Url> {
    if specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../") {
        return base.join(specifier).ok();
    }
    Url::parse(specifier).ok()
}

/// HTML's "resolve an imports match". `None` means no key matched.
fn resolve_imports_match(
    normalized: &str,
    as_url: Option<&Url>,
    map: &SpecifierMap,
) -> Option<Result<Url, ImportMapResolveError>> {
    for (key, address) in map {
        if key == normalized {
            return Some(address.clone().ok_or_else(|| blocked(normalized)));
        }
        let special = as_url.is_none_or(|url| is_special_scheme(url.scheme()));
        if key.ends_with('/') && normalized.starts_with(key.as_str()) && special {
            let Some(address) = address else {
                return Some(Err(blocked(normalized)));
            };
            let after_prefix = &normalized[key.len()..];
            let Ok(resolved) = address.join(after_prefix) else {
                return Some(Err(ImportMapResolveError {
                    message: format!(
                        "the import map entry \"{key}\" cannot resolve \"{normalized}\""
                    ),
                }));
            };
            if !resolved.as_str().starts_with(address.as_str()) {
                return Some(Err(ImportMapResolveError {
                    message: format!(
                        "the import map entry \"{key}\" resolves \"{normalized}\" outside its \
                         address (backtracking)"
                    ),
                }));
            }
            return Some(Ok(resolved));
        }
    }
    None
}

fn blocked(specifier: &str) -> ImportMapResolveError {
    ImportMapResolveError {
        message: format!("the import map blocks the module specifier \"{specifier}\""),
    }
}

fn is_special_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "file" | "ftp" | "ws" | "wss")
}

/// HTML's "sort and normalize a specifier map".
fn normalize_specifier_map(
    map: &serde_json::Map<String, serde_json::Value>,
    base: &Url,
    warnings: &mut Vec<String>,
) -> SpecifierMap {
    let mut normalized: SpecifierMap = Vec::new();
    for (key, value) in map {
        if key.is_empty() {
            warnings.push("ignored an empty import map specifier key".to_owned());
            continue;
        }
        let normalized_key =
            parse_url_like_specifier(key, base).map_or_else(|| key.clone(), |url| url.to_string());
        let serde_json::Value::String(address) = value else {
            warnings.push(format!("the address of \"{key}\" is not a string"));
            normalized.push((normalized_key, None));
            continue;
        };
        let Some(address_url) = parse_url_like_specifier(address, base) else {
            warnings.push(format!(
                "the address \"{address}\" of \"{key}\" is not a URL"
            ));
            normalized.push((normalized_key, None));
            continue;
        };
        if key.ends_with('/') && !address_url.as_str().ends_with('/') {
            warnings.push(format!(
                "the address \"{address}\" of the prefix \"{key}\" must end with \"/\""
            ));
            normalized.push((normalized_key, None));
            continue;
        }
        normalized.retain(|(existing, _)| *existing != normalized_key);
        normalized.push((normalized_key, Some(address_url)));
    }
    normalized.sort_by(|a, b| b.0.cmp(&a.0));
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/app/index.html").unwrap()
    }

    fn map(text: &str) -> ImportMap {
        ImportMap::parse(text, "https://example.com/app/index.html")
            .unwrap()
            .0
    }

    #[test]
    fn relative_and_absolute_specifiers_resolve_without_a_map() {
        let empty = ImportMap::default();
        assert_eq!(
            empty.resolve("./a.js", &base()).unwrap().as_str(),
            "https://example.com/app/a.js"
        );
        assert_eq!(
            empty.resolve("/b.js", &base()).unwrap().as_str(),
            "https://example.com/b.js"
        );
        assert_eq!(
            empty
                .resolve("https://cdn.test/x.js", &base())
                .unwrap()
                .as_str(),
            "https://cdn.test/x.js"
        );
        assert!(empty.resolve("lodash", &base()).is_err());
    }

    #[test]
    fn bare_and_prefix_specifiers_map() {
        let map = map(r#"{"imports": {"vue": "/lib/vue.js", "lib/": "https://cdn.test/lib/"}}"#);
        assert_eq!(
            map.resolve("vue", &base()).unwrap().as_str(),
            "https://example.com/lib/vue.js"
        );
        assert_eq!(
            map.resolve("lib/a/b.js", &base()).unwrap().as_str(),
            "https://cdn.test/lib/a/b.js"
        );
        assert!(map.resolve("lib/../../escape.js", &base()).is_err());
    }

    #[test]
    fn scopes_win_over_top_level_imports() {
        let map = map(r#"{"imports": {"dep": "/v1/dep.js"},
                "scopes": {"/app/legacy/": {"dep": "/v0/dep.js"}}}"#);
        let legacy = Url::parse("https://example.com/app/legacy/main.js").unwrap();
        assert_eq!(
            map.resolve("dep", &legacy).unwrap().as_str(),
            "https://example.com/v0/dep.js"
        );
        assert_eq!(
            map.resolve("dep", &base()).unwrap().as_str(),
            "https://example.com/v1/dep.js"
        );
    }

    #[test]
    fn url_keys_remap_full_urls() {
        let map = map(r#"{"imports": {"https://cdn.test/old.js": "./new.js"}}"#);
        assert_eq!(
            map.resolve("https://cdn.test/old.js", &base())
                .unwrap()
                .as_str(),
            "https://example.com/app/new.js"
        );
    }

    #[test]
    fn invalid_addresses_block_and_warn() {
        let (map, warnings) = ImportMap::parse(
            r#"{"imports": {"a": 1, "b/": "/no-slash"}}"#,
            "https://example.com/",
        )
        .unwrap();
        assert_eq!(warnings.len(), 2);
        assert!(map.resolve("a", &base()).is_err());
        assert!(map.resolve("b/x.js", &base()).is_err());
    }

    #[test]
    fn malformed_maps_are_errors() {
        assert!(ImportMap::parse("not json", "https://example.com/").is_err());
        assert!(ImportMap::parse("[]", "https://example.com/").is_err());
        assert!(ImportMap::parse(r#"{"imports": []}"#, "https://example.com/").is_err());
    }

    #[test]
    fn merging_keeps_earlier_rules() {
        let mut first = map(r#"{"imports": {"a": "/a1.js"}}"#);
        first.merge(map(r#"{"imports": {"a": "/a2.js", "b": "/b.js"}}"#));
        assert_eq!(
            first.resolve("a", &base()).unwrap().as_str(),
            "https://example.com/a1.js"
        );
        assert_eq!(
            first.resolve("b", &base()).unwrap().as_str(),
            "https://example.com/b.js"
        );
    }
}
