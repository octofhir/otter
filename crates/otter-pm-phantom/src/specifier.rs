//! Classification of module specifiers.
//!
//! Only one shape of specifier can name an undeclared dependency: a bare
//! package specifier. Relative paths, absolute paths, URLs, and the `#`
//! subpath-imports namespace all resolve without a `package.json` dependency
//! entry, so they are excluded before any classification happens.
//!
//! # Contents
//! - [`package_name`] — the package a bare specifier belongs to.
//!
//! # Invariants
//! - A scoped specifier keeps both segments: `@scope/pkg/sub` belongs to
//!   `@scope/pkg`, never to `@scope`.
//! - Anything carrying a scheme, a leading `.`, `/`, or `#` yields `None`; it
//!   is resolvable without a dependency declaration.
//!
//! # See also
//! - [`crate::builtins`] for the builtin check that follows.

/// The package a bare specifier belongs to, or `None` when the specifier is
/// not a bare package specifier at all.
#[must_use]
pub fn package_name(specifier: &str) -> Option<String> {
    if specifier.is_empty()
        || specifier.starts_with('.')
        || specifier.starts_with('/')
        || specifier.starts_with('#')
        || specifier.contains(':')
    {
        return None;
    }
    let mut parts = specifier.split('/');
    let head = parts.next()?;
    if head.starts_with('@') {
        let scope_member = parts.next()?;
        if head.len() < 2 || scope_member.is_empty() {
            return None;
        }
        return Some(format!("{head}/{scope_member}"));
    }
    if head.is_empty() {
        return None;
    }
    Some(head.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_specifier_yields_its_package() {
        assert_eq!(package_name("left-pad").as_deref(), Some("left-pad"));
        assert_eq!(package_name("lodash/fp").as_deref(), Some("lodash"));
    }

    #[test]
    fn a_scoped_specifier_keeps_both_segments() {
        assert_eq!(
            package_name("@scope/pkg/sub/path").as_deref(),
            Some("@scope/pkg")
        );
        assert_eq!(package_name("@scope").as_deref(), None);
    }

    #[test]
    fn specifiers_that_resolve_without_a_dependency_yield_nothing() {
        assert_eq!(package_name("./local.js"), None);
        assert_eq!(package_name("../up.js"), None);
        assert_eq!(package_name("/abs/path.js"), None);
        assert_eq!(package_name("#internal"), None);
        assert_eq!(package_name("node:fs"), None);
        assert_eq!(package_name("https://example.com/mod.js"), None);
        assert_eq!(package_name("data:text/javascript,1"), None);
        assert_eq!(package_name(""), None);
    }
}
