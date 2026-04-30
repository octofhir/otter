//! Test262 `features:` token → engine readiness bucket.
//!
//! **Single source of truth: `test262_config.toml`.** The runner is
//! deliberately empty of hard-coded "ready" / "not-ready" feature
//! lists — every entry the project wants to skip lives in
//! `skip_features = [...]` in the TOML config. Editing the config is
//! the *only* way to add or remove a skip; no Rust code change is
//! required.
//!
//! Spec links:
//! - <https://github.com/tc39/test262/blob/main/INTERPRETING.md#features>
//! - `test262_config.toml` (root of the repository)

use std::collections::BTreeSet;

/// Engine readiness bucket for a single Test262 feature token.
///
/// `Skip` = the feature appears in `skip_features` in the config.
/// `Grade` = the feature does not appear in the skip list and so the
/// runner attempts to grade tests that depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Readiness {
    /// Feature is in the config's `skip_features` list — tests that
    /// require it are reported as `Skipped(<feature>)`.
    Skip,
    /// Feature is not in the skip list — the runner grades tests
    /// that require it.
    Grade,
}

/// Config-driven feature readiness map. Built from the `skip_features`
/// list in `test262_config.toml`.
#[derive(Debug, Clone, Default)]
pub struct FeatureMap {
    skip: BTreeSet<String>,
}

impl FeatureMap {
    /// Build a map from a `skip_features` list (the array carried in
    /// `test262_config.toml`).
    #[must_use]
    pub fn from_skip_features<I, S>(skip_features: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            skip: skip_features.into_iter().map(Into::into).collect(),
        }
    }

    /// Number of distinct skip tokens.
    #[must_use]
    pub fn len(&self) -> usize {
        self.skip.len()
    }

    /// `true` when the skip list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skip.is_empty()
    }

    /// `Skip` if `feature` is on the skip list, `Grade` otherwise.
    #[must_use]
    pub fn lookup(&self, feature: &str) -> Readiness {
        if self.skip.contains(feature) {
            Readiness::Skip
        } else {
            Readiness::Grade
        }
    }

    /// Walk `features` and return the first token that is in the
    /// skip list. The runner uses the returned name as the
    /// `Skipped` reason.
    #[must_use]
    pub fn first_skipped<'a>(&self, features: &'a [String]) -> Option<&'a str> {
        features
            .iter()
            .find(|f| self.skip.contains(f.as_str()))
            .map(String::as_str)
    }

    /// Iterate the skip list in alphabetical order.
    pub fn iter_skipped(&self) -> impl Iterator<Item = &str> {
        self.skip.iter().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_grades_when_token_absent() {
        let map = FeatureMap::default();
        assert_eq!(map.lookup("BigInt"), Readiness::Grade);
        assert!(map.is_empty());
    }

    #[test]
    fn lookup_skips_when_token_in_skip_list() {
        let map = FeatureMap::from_skip_features(["Atomics", "ShadowRealm"]);
        assert_eq!(map.lookup("Atomics"), Readiness::Skip);
        assert_eq!(map.lookup("ShadowRealm"), Readiness::Skip);
        assert_eq!(map.lookup("BigInt"), Readiness::Grade);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn first_skipped_picks_blocker_in_iteration_order() {
        let map = FeatureMap::from_skip_features(["Atomics", "Temporal"]);
        let want = vec![
            "BigInt".to_string(),
            "Temporal".to_string(),
            "class".to_string(),
        ];
        assert_eq!(map.first_skipped(&want), Some("Temporal"));

        let none = vec!["BigInt".to_string(), "class".to_string()];
        assert_eq!(map.first_skipped(&none), None);
    }
}
