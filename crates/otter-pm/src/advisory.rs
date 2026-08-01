//! Malicious-package advisory lookups.
//!
//! Integrity verification proves a tarball is the one the registry published;
//! it says nothing about whether that tarball should exist. This module asks
//! the other question — is this exact `(name, version)` a package the
//! ecosystem has already flagged as malicious — against a public advisory
//! database, before any of its bytes are unpacked into a project.
//!
//! # Contents
//! - [`AdvisoryQuery`] — one package version to ask about.
//! - [`MaliciousAdvisory`] — one confirmed-malicious answer.
//! - [`AdvisoryClient`] — the async lookup interface.
//! - [`HttpAdvisoryClient`] — the public-database implementation.
//! - [`FixtureAdvisoryClient`] — deterministic offline answers for tests.
//!
//! # Invariants
//! - Only advisories that name a package as malicious are reported. Ordinary
//!   vulnerability advisories are a different question with a different answer
//!   and are not a reason to refuse an install.
//! - The database is consulted with a tight timeout; a slow lookup must not
//!   turn a local install into a network wait.
//! - Availability and verdict are distinct results: an unreachable database
//!   is an error the caller's policy decides about, never a silent "clean".
//!
//! # See also
//! - [`crate::policy::InstallPolicy::advisory_check`] for what a failed
//!   lookup costs.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::PackageManagerError;

/// Public batch-query endpoint of the advisory database.
const DEFAULT_ADVISORY_ENDPOINT: &str = "https://api.osv.dev/v1/querybatch";

/// Ecosystem name the advisory database indexes npm packages under.
const NPM_ECOSYSTEM: &str = "npm";

/// Advisory identifier prefix reserved for confirmed-malicious packages.
const MALICIOUS_ID_PREFIX: &str = "MAL-";

/// Queries per batch request. The database documents a larger ceiling; this
/// leaves headroom so a large dependency graph chunks instead of truncating.
const BATCH_LIMIT: usize = 500;

/// Lookup timeout. These are gates on a local operation, so a slow mirror
/// must surface as an unavailable database rather than a stalled install.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);

/// One package version to ask the advisory database about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AdvisoryQuery {
    /// Package name.
    pub name: String,
    /// Resolved version.
    pub version: String,
}

/// One confirmed-malicious package version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MaliciousAdvisory {
    /// Package name.
    pub name: String,
    /// Affected version.
    pub version: String,
    /// Advisory identifier.
    pub id: String,
}

/// Source of malicious-package advisories.
pub trait AdvisoryClient {
    /// Look up every query, returning only confirmed-malicious matches.
    fn query_malicious<'a>(
        &'a self,
        queries: &'a [AdvisoryQuery],
    ) -> Pin<
        Box<dyn Future<Output = Result<Vec<MaliciousAdvisory>, PackageManagerError>> + Send + 'a>,
    >;
}

/// Advisory client backed by the public database.
#[derive(Debug, Clone)]
pub struct HttpAdvisoryClient {
    client: reqwest::Client,
    endpoint: String,
}

impl HttpAdvisoryClient {
    /// Build a client against the public advisory database.
    #[must_use]
    pub fn new() -> Self {
        Self::with_endpoint(DEFAULT_ADVISORY_ENDPOINT)
    }

    /// Build a client against an explicit batch-query endpoint.
    #[must_use]
    pub fn with_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: endpoint.into(),
        }
    }

    async fn query_chunk(
        &self,
        chunk: &[AdvisoryQuery],
    ) -> Result<Vec<MaliciousAdvisory>, PackageManagerError> {
        let request = BatchRequest {
            queries: chunk
                .iter()
                .map(|query| BatchQuery {
                    package: QueryPackage {
                        name: query.name.clone(),
                        ecosystem: NPM_ECOSYSTEM.to_string(),
                    },
                    version: query.version.clone(),
                })
                .collect(),
        };
        let response = self
            .client
            .post(&self.endpoint)
            .timeout(LOOKUP_TIMEOUT)
            .json(&request)
            .send()
            .await
            .map_err(|err| PackageManagerError::AdvisoryUnavailable {
                message: err.to_string(),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(PackageManagerError::AdvisoryUnavailable {
                message: format!("advisory database returned {status}"),
            });
        }
        let body: BatchResponse =
            response
                .json()
                .await
                .map_err(|err| PackageManagerError::AdvisoryUnavailable {
                    message: err.to_string(),
                })?;
        Ok(malicious_from_results(chunk, &body))
    }
}

impl Default for HttpAdvisoryClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AdvisoryClient for HttpAdvisoryClient {
    fn query_malicious<'a>(
        &'a self,
        queries: &'a [AdvisoryQuery],
    ) -> Pin<
        Box<dyn Future<Output = Result<Vec<MaliciousAdvisory>, PackageManagerError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut found = Vec::new();
            for chunk in queries.chunks(BATCH_LIMIT) {
                found.extend(self.query_chunk(chunk).await?);
            }
            found.sort();
            found.dedup();
            Ok(found)
        })
    }
}

/// Advisory client with fixed answers, for deterministic offline tests.
#[derive(Debug, Clone, Default)]
pub struct FixtureAdvisoryClient {
    advisories: Vec<MaliciousAdvisory>,
    unavailable: Option<String>,
}

impl FixtureAdvisoryClient {
    /// Build a client that reports the given advisories and nothing else.
    #[must_use]
    pub fn new(advisories: Vec<MaliciousAdvisory>) -> Self {
        Self {
            advisories,
            unavailable: None,
        }
    }

    /// Build a client that always fails to answer.
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            advisories: Vec::new(),
            unavailable: Some(message.into()),
        }
    }
}

impl AdvisoryClient for FixtureAdvisoryClient {
    fn query_malicious<'a>(
        &'a self,
        queries: &'a [AdvisoryQuery],
    ) -> Pin<
        Box<dyn Future<Output = Result<Vec<MaliciousAdvisory>, PackageManagerError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if let Some(message) = &self.unavailable {
                return Err(PackageManagerError::AdvisoryUnavailable {
                    message: message.clone(),
                });
            }
            Ok(self
                .advisories
                .iter()
                .filter(|advisory| {
                    queries.iter().any(|query| {
                        query.name == advisory.name && query.version == advisory.version
                    })
                })
                .cloned()
                .collect())
        })
    }
}

fn malicious_from_results(
    chunk: &[AdvisoryQuery],
    response: &BatchResponse,
) -> Vec<MaliciousAdvisory> {
    let mut found = Vec::new();
    for (query, result) in chunk.iter().zip(response.results.iter()) {
        for vulnerability in &result.vulns {
            if vulnerability.id.starts_with(MALICIOUS_ID_PREFIX) {
                found.push(MaliciousAdvisory {
                    name: query.name.clone(),
                    version: query.version.clone(),
                    id: vulnerability.id.clone(),
                });
            }
        }
    }
    found
}

#[derive(Debug, Serialize)]
struct BatchRequest {
    queries: Vec<BatchQuery>,
}

#[derive(Debug, Serialize)]
struct BatchQuery {
    package: QueryPackage,
    version: String,
}

#[derive(Debug, Serialize)]
struct QueryPackage {
    name: String,
    ecosystem: String,
}

#[derive(Debug, Default, Deserialize)]
struct BatchResponse {
    #[serde(default)]
    results: Vec<BatchResult>,
}

#[derive(Debug, Default, Deserialize)]
struct BatchResult {
    #[serde(default)]
    vulns: Vec<BatchVulnerability>,
}

#[derive(Debug, Deserialize)]
struct BatchVulnerability {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, version: &str) -> AdvisoryQuery {
        AdvisoryQuery {
            name: name.to_string(),
            version: version.to_string(),
        }
    }

    #[test]
    fn only_malicious_advisories_are_reported() {
        let chunk = [query("left-pad", "1.0.0"), query("tool", "2.0.0")];
        let response: BatchResponse = serde_json::from_str(
            r#"{"results":[
                {"vulns":[{"id":"GHSA-1111-2222-3333"}]},
                {"vulns":[{"id":"MAL-2026-42"},{"id":"GHSA-4444"}]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            malicious_from_results(&chunk, &response),
            [MaliciousAdvisory {
                name: "tool".to_string(),
                version: "2.0.0".to_string(),
                id: "MAL-2026-42".to_string(),
            }]
        );
    }

    #[test]
    fn an_empty_result_row_reports_nothing() {
        let chunk = [query("tool", "2.0.0")];
        let response: BatchResponse = serde_json::from_str(r#"{"results":[{}]}"#).unwrap();
        assert!(malicious_from_results(&chunk, &response).is_empty());
    }

    #[tokio::test]
    async fn the_fixture_client_answers_only_for_queried_versions() {
        let client = FixtureAdvisoryClient::new(vec![MaliciousAdvisory {
            name: "tool".to_string(),
            version: "2.0.0".to_string(),
            id: "MAL-2026-42".to_string(),
        }]);
        assert!(
            client
                .query_malicious(&[query("tool", "1.0.0")])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            client
                .query_malicious(&[query("tool", "2.0.0")])
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn an_unavailable_database_is_an_error_not_a_clean_answer() {
        let client = FixtureAdvisoryClient::unavailable("connection refused");
        let error = client
            .query_malicious(&[query("tool", "1.0.0")])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PackageManagerError::AdvisoryUnavailable { .. }
        ));
    }
}
