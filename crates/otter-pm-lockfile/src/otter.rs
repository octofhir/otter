//! Otter-native lockfile adapter.
//!
//! Two on-disk shapes are accepted:
//! - `otter-lock.yaml` (new default) — same wire format, YAML-encoded
//! - `otter.lock` (legacy JSON) — the format [`crate::LockfileGraph`] was
//!   bootstrapped on; kept for backwards-compatible reads.
//!
//! Both go through the same [`GraphWire`] projection so a file that
//! round-trips parse → write is byte-identical.
//!
//! When Phase 2 lands the `pnpm` adapter, `otter-lock.yaml` gains the
//! full importer / settings / overrides round-trip surface. The Phase 1
//! shape is deliberately minimal — enough to let the existing
//! [`otter_pm::Installer`] keep working on top of [`LockfileGraph`].

use crate::{
    DepType, DirectDep, DirectDepWire, Error, GraphWire, LockedPackage, LockedPackageWire,
    LockfileGraph,
};
use otter_pm_manifest::PackageJson;
use std::collections::BTreeMap;
use std::path::Path;

/// Parse an otter-lock file at `path`. Dispatches on the extension:
/// `.yaml` → YAML; anything else → JSON (covers the legacy `otter.lock`).
pub fn parse(path: &Path, _manifest: &PackageJson) -> Result<LockfileGraph, Error> {
    let bytes = std::fs::read(path).map_err(|e| Error::Io(path.to_path_buf(), e.to_string()))?;
    let is_yaml = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
        .unwrap_or(false);

    if is_yaml {
        let wire: OwnedGraphWire = serde_yaml::from_slice(&bytes)
            .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;
        Ok(from_wire(wire))
    } else {
        let wire: OwnedGraphWire = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;
        Ok(from_wire(wire))
    }
}

/// Write `graph` to `path`. Dispatches on the extension: `.yaml` → YAML
/// with a trailing newline; anything else → pretty JSON (covers the
/// legacy `otter.lock`).
pub fn write(path: &Path, graph: &LockfileGraph, _manifest: &PackageJson) -> Result<(), Error> {
    let is_yaml = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
        .unwrap_or(false);

    let wire = to_wire(graph);
    let mut content = if is_yaml {
        serde_yaml::to_string(&wire).map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?
    } else {
        serde_json::to_string_pretty(&wire)
            .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?
    };
    if !content.ends_with('\n') {
        content.push('\n');
    }
    std::fs::write(path, content).map_err(|e| Error::Io(path.to_path_buf(), e.to_string()))
}

/// Serialize `graph` to canonical JSON bytes. Used by
/// [`LockfileGraph::checksum`] so hashing is independent of the
/// format adapter in use at write time.
pub fn to_canonical_json(graph: &LockfileGraph) -> String {
    let wire = to_wire(graph);
    serde_json::to_string(&wire).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Wire ↔ graph mapping
// ---------------------------------------------------------------------------

fn to_wire(graph: &LockfileGraph) -> GraphWire<'_> {
    let importers: BTreeMap<String, Vec<DirectDepWire<'_>>> = graph
        .importers
        .iter()
        .map(|(path, deps)| {
            (
                path.clone(),
                deps.iter()
                    .map(|d| DirectDepWire {
                        name: &d.name,
                        dep_path: &d.dep_path,
                        dep_type: d.dep_type.as_wire(),
                        specifier: d.specifier.as_deref(),
                    })
                    .collect(),
            )
        })
        .collect();

    let packages: BTreeMap<String, LockedPackageWire<'_>> = graph
        .packages
        .iter()
        .map(|(k, p)| {
            (
                k.clone(),
                LockedPackageWire {
                    name: &p.name,
                    version: &p.version,
                    integrity: p.integrity.as_deref(),
                    dependencies: p
                        .dependencies
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect(),
                    optional_dependencies: p
                        .optional_dependencies
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect(),
                    peer_dependencies: p
                        .peer_dependencies
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect(),
                    tarball_url: p.tarball_url.as_deref(),
                },
            )
        })
        .collect();

    GraphWire {
        version: OTTER_WIRE_VERSION,
        importers,
        packages,
        overrides: graph.overrides.clone(),
        ignored_optional_dependencies: graph.ignored_optional_dependencies.clone(),
        times: graph.times.clone(),
    }
}

fn from_wire(w: OwnedGraphWire) -> LockfileGraph {
    let importers: BTreeMap<String, Vec<DirectDep>> = w
        .importers
        .into_iter()
        .map(|(path, deps)| {
            (
                path,
                deps.into_iter()
                    .map(|d| DirectDep {
                        name: d.name,
                        dep_path: d.dep_path,
                        dep_type: DepType::from_wire(&d.dep_type),
                        specifier: d.specifier,
                    })
                    .collect(),
            )
        })
        .collect();

    let packages: BTreeMap<String, LockedPackage> = w
        .packages
        .into_iter()
        .map(|(k, p)| {
            let lp = LockedPackage {
                name: p.name,
                version: p.version,
                integrity: p.integrity,
                dependencies: p.dependencies.into_iter().collect(),
                optional_dependencies: p.optional_dependencies.into_iter().collect(),
                peer_dependencies: p.peer_dependencies.into_iter().collect(),
                dep_path: k.clone(),
                tarball_url: p.tarball_url,
                ..LockedPackage::default()
            };
            (k, lp)
        })
        .collect();

    LockfileGraph {
        importers,
        packages,
        overrides: w.overrides,
        ignored_optional_dependencies: w.ignored_optional_dependencies,
        times: w.times,
        ..LockfileGraph::default()
    }
}

/// Wire-format major version written to disk. Bumped on breaking
/// layout changes (renamed / removed fields). Adding an optional field
/// is *not* breaking — serde defaults cover it.
const OTTER_WIRE_VERSION: u32 = 1;

// Owned mirror of `GraphWire`, used for deserialization. We can't
// deserialize into the borrowed `GraphWire<'a>` directly because the
// source bytes aren't borrowed for the right lifetime.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct OwnedGraphWire {
    #[serde(default = "default_wire_version")]
    version: u32,
    #[serde(default)]
    importers: BTreeMap<String, Vec<OwnedDirectDepWire>>,
    #[serde(default)]
    packages: BTreeMap<String, OwnedLockedPackageWire>,
    #[serde(default)]
    overrides: BTreeMap<String, String>,
    #[serde(default)]
    ignored_optional_dependencies: std::collections::BTreeSet<String>,
    #[serde(default)]
    times: BTreeMap<String, String>,
}

fn default_wire_version() -> u32 {
    OTTER_WIRE_VERSION
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct OwnedDirectDepWire {
    name: String,
    dep_path: String,
    dep_type: String,
    #[serde(default)]
    specifier: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct OwnedLockedPackageWire {
    name: String,
    version: String,
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    tarball_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_graph() -> LockfileGraph {
        let mut g = LockfileGraph::default();
        g.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "is-odd".to_string(),
                dep_path: "is-odd@3.0.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^3".to_string()),
            }],
        );
        g.packages.insert(
            "is-odd@3.0.1".to_string(),
            LockedPackage {
                name: "is-odd".to_string(),
                version: "3.0.1".to_string(),
                integrity: Some("sha512-stub".to_string()),
                dep_path: "is-odd@3.0.1".to_string(),
                ..LockedPackage::default()
            },
        );
        g
    }

    #[test]
    fn json_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("otter.lock");
        let g1 = seed_graph();
        let manifest = PackageJson::default();
        write(&path, &g1, &manifest).unwrap();
        let g2 = parse(&path, &manifest).unwrap();
        assert_eq!(g1.checksum(), g2.checksum());
    }

    #[test]
    fn yaml_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("otter-lock.yaml");
        let g1 = seed_graph();
        let manifest = PackageJson::default();
        write(&path, &g1, &manifest).unwrap();
        let g2 = parse(&path, &manifest).unwrap();
        assert_eq!(g1.checksum(), g2.checksum());
    }

    #[test]
    fn canonical_json_is_deterministic_across_insertion_orders() {
        let mut a = LockfileGraph::default();
        for (name, version) in [("zeta", "1.0.0"), ("alpha", "2.0.0"), ("mu", "3.0.0")] {
            let key = format!("{name}@{version}");
            a.packages.insert(
                key.clone(),
                LockedPackage {
                    name: name.to_string(),
                    version: version.to_string(),
                    dep_path: key,
                    ..LockedPackage::default()
                },
            );
        }
        let mut b = LockfileGraph::default();
        for (name, version) in [("mu", "3.0.0"), ("alpha", "2.0.0"), ("zeta", "1.0.0")] {
            let key = format!("{name}@{version}");
            b.packages.insert(
                key.clone(),
                LockedPackage {
                    name: name.to_string(),
                    version: version.to_string(),
                    dep_path: key,
                    ..LockedPackage::default()
                },
            );
        }
        assert_eq!(to_canonical_json(&a), to_canonical_json(&b));
        assert_eq!(a.checksum(), b.checksum());
    }
}
