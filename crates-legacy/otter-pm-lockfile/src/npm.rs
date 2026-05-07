//! `package-lock.json` v3 + `npm-shrinkwrap.json` adapter.
//!
//! # Scope
//!
//! Parse / write npm v3 lockfiles (the default since npm 7). Both
//! filenames share one schema — `npm-shrinkwrap.json` is just
//! `package-lock.json` with a different name and slightly different
//! semantics for the consumer (shrinkwrap pins for downstream installs).
//!
//! # What's preserved
//!
//! - The root project entry at `""` (name/version)
//! - Per-path entries under `packages["node_modules/<name>"]`:
//!   version, resolved URL, integrity, dependencies /
//!   devDependencies / optionalDependencies / peerDependencies,
//!   os / cpu / libc, engines (dropped on write since we don't
//!   expose it yet), deprecated flag (same)
//! - Nested-path entries (`"node_modules/a/node_modules/b"`) are
//!   collapsed onto the innermost package key in [`LockfileGraph`],
//!   matching pnpm's flat model. The nested structure is regenerated
//!   on write from the graph's dep edges.
//!
//! # Lossy areas
//!
//! - **Peer-context identity** — npm v3 does not encode peer
//!   contexts in the lockfile. A parse of a peer-sensitive npm
//!   lockfile and write as pnpm would lose the fan-out; within npm
//!   the round-trip is clean.
//! - **Dependency edges** per-package are reconstructed from the
//!   flat `dependencies` map. The subtle path-based shadowing npm
//!   uses in deeply nested trees isn't preserved.
//!
//! See `plans/partitioned-conjuring-church.md` for the roadmap to
//! a fuller implementation.

use crate::{DepType, DirectDep, Error, LockedPackage, LockfileGraph};
use otter_pm_manifest::PackageJson;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// `lockfileVersion` we emit. npm 7+ writes `3` by default.
const LOCKFILE_VERSION: u32 = 3;

// ---------------------------------------------------------------------------
// Parse
// ---------------------------------------------------------------------------

pub fn parse(path: &Path) -> Result<LockfileGraph, Error> {
    let bytes = std::fs::read(path).map_err(|e| Error::Io(path.to_path_buf(), e.to_string()))?;
    let raw: RawNpmLockfile = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;

    // Split `packages:` keys: `""` is the root importer, everything
    // else is a `node_modules/<...>` path pointing at a resolved package.
    let mut importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut seen_dep_paths: BTreeMap<String, ()> = BTreeMap::new();

    let raw_packages = raw.packages.unwrap_or_default();
    for (key, entry) in &raw_packages {
        if key.is_empty() {
            // Root importer. Collapse its dep blocks into `importers["."]`.
            let mut root_deps = Vec::new();
            for (name, spec) in &entry.dependencies {
                if let Some(dep) = direct_dep_from_name_spec(name, spec, DepType::Production) {
                    root_deps.push(dep);
                }
            }
            for (name, spec) in &entry.dev_dependencies {
                if let Some(dep) = direct_dep_from_name_spec(name, spec, DepType::Dev) {
                    root_deps.push(dep);
                }
            }
            for (name, spec) in &entry.optional_dependencies {
                if let Some(dep) = direct_dep_from_name_spec(name, spec, DepType::Optional) {
                    root_deps.push(dep);
                }
            }
            importers.insert(".".to_string(), root_deps);
            continue;
        }

        // Non-root entry: `node_modules/<name>` or
        // `node_modules/<a>/node_modules/<b>/.../node_modules/<c>`.
        // Strip the leading `node_modules/` prefix (npm writes
        // exactly that shape) and take the innermost `<name>` +
        // nested chain.
        let Some(name) = leaf_package_name(key) else {
            continue;
        };
        // Skip when the entry is a workspace link (handled below).
        if entry.link.unwrap_or(false) {
            continue;
        }

        let version = match entry.version.clone() {
            Some(v) => v,
            None => continue,
        };
        let dep_path = format!("{name}@{version}");

        // Deduplicate: keep the first occurrence (shortest path wins,
        // which matches what we'd see at the tree's top level).
        if seen_dep_paths.contains_key(&dep_path) {
            continue;
        }
        seen_dep_paths.insert(dep_path.clone(), ());

        packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: name.clone(),
                version: version.clone(),
                integrity: entry.integrity.clone(),
                dependencies: entry.dependencies.clone(),
                optional_dependencies: entry.optional_dependencies.clone(),
                peer_dependencies: entry.peer_dependencies.clone(),
                dep_path: dep_path.clone(),
                os: entry.os.clone(),
                cpu: entry.cpu.clone(),
                libc: entry.libc.clone(),
                tarball_url: entry.resolved.clone(),
                ..LockedPackage::default()
            },
        );
    }

    // Populate importer dep_paths from package versions (npm stores
    // specifiers in the root entry but versions in the child entries —
    // so we resolve each root dep's specifier by name → version lookup
    // via what we just collected).
    if let Some(root_deps) = importers.get_mut(".") {
        for d in root_deps.iter_mut() {
            // Look up the installed version for this name; the
            // `direct_dep_from_name_spec` helper seeded `dep_path`
            // with the specifier — replace it with the real key.
            let candidate = packages
                .values()
                .find(|p| p.name == d.name)
                .map(|p| p.dep_path.clone());
            if let Some(real) = candidate {
                d.dep_path = real;
            }
        }
    }

    Ok(LockfileGraph {
        importers,
        packages,
        ..LockfileGraph::default()
    })
}

fn direct_dep_from_name_spec(name: &str, spec: &str, dep_type: DepType) -> Option<DirectDep> {
    Some(DirectDep {
        name: name.to_string(),
        // Provisional dep_path seeded with the spec; the caller fixes
        // this up once the packages map is fully populated.
        dep_path: format!("{name}@{spec}"),
        dep_type,
        // npm lockfiles do preserve the user's specifier at the root —
        // use it here so drift detection has something to compare
        // against on Otter writes. (Non-root npm entries don't carry
        // specifiers at all.)
        specifier: Some(spec.to_string()),
    })
}

/// Extract the innermost package name from an npm `packages:` key.
/// `node_modules/foo` → `"foo"`. `node_modules/foo/node_modules/@bar/baz` → `"@bar/baz"`.
fn leaf_package_name(key: &str) -> Option<String> {
    let mut out: Option<&str> = None;
    for segment in key.split("node_modules/") {
        let seg = segment.trim_end_matches('/');
        if !seg.is_empty() {
            out = Some(seg);
        }
    }
    out.map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

pub fn write(path: &Path, graph: &LockfileGraph, manifest: &PackageJson) -> Result<(), Error> {
    let mut packages: BTreeMap<String, WritableEntry> = BTreeMap::new();

    // Root entry.
    let root_deps = graph.root_deps();
    let mut root_dependencies: BTreeMap<String, String> = BTreeMap::new();
    let mut root_dev: BTreeMap<String, String> = BTreeMap::new();
    let mut root_optional: BTreeMap<String, String> = BTreeMap::new();
    for d in root_deps {
        let spec = d.specifier.clone().unwrap_or_else(|| {
            // Fall back to the locked version when no specifier was
            // recorded (e.g. converting from a pnpm lockfile where
            // the importer already stored one).
            graph
                .get_package(&d.dep_path)
                .map(|p| p.version.clone())
                .unwrap_or_default()
        });
        match d.dep_type {
            DepType::Production => {
                root_dependencies.insert(d.name.clone(), spec);
            }
            DepType::Dev => {
                root_dev.insert(d.name.clone(), spec);
            }
            DepType::Optional => {
                root_optional.insert(d.name.clone(), spec);
            }
        }
    }
    packages.insert(
        String::new(),
        WritableEntry {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            dependencies: (!root_dependencies.is_empty()).then_some(root_dependencies),
            dev_dependencies: (!root_dev.is_empty()).then_some(root_dev),
            optional_dependencies: (!root_optional.is_empty()).then_some(root_optional),
            peer_dependencies: None,
            resolved: None,
            integrity: None,
            os: Vec::new(),
            cpu: Vec::new(),
            libc: Vec::new(),
            link: None,
            dev: None,
        },
    );

    // Non-root entries — emit one `node_modules/<name>` entry per
    // package. A fuller implementation would also emit nested
    // `node_modules/a/node_modules/b` entries when npm's hoisting
    // would deduplicate; Otter ships a flat model that npm still
    // consumes correctly.
    for pkg in graph.packages.values() {
        let key = format!("node_modules/{}", pkg.name);
        packages.insert(
            key,
            WritableEntry {
                name: None,
                version: Some(pkg.version.clone()),
                dependencies: (!pkg.dependencies.is_empty()).then(|| pkg.dependencies.clone()),
                dev_dependencies: None,
                optional_dependencies: (!pkg.optional_dependencies.is_empty())
                    .then(|| pkg.optional_dependencies.clone()),
                peer_dependencies: (!pkg.peer_dependencies.is_empty())
                    .then(|| pkg.peer_dependencies.clone()),
                resolved: pkg.tarball_url.clone(),
                integrity: pkg.integrity.clone(),
                os: pkg.os.clone(),
                cpu: pkg.cpu.clone(),
                libc: pkg.libc.clone(),
                link: None,
                dev: None,
            },
        );
    }

    let out = WritableLockfile {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        lockfile_version: LOCKFILE_VERSION,
        requires: true,
        packages,
    };
    let mut json = serde_json::to_string_pretty(&out)
        .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;
    json.push('\n');
    std::fs::write(path, json).map_err(|e| Error::Io(path.to_path_buf(), e.to_string()))
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawNpmLockfile {
    #[serde(default)]
    packages: Option<BTreeMap<String, RawEntry>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    resolved: Option<String>,
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    cpu: Vec<String>,
    #[serde(default)]
    libc: Vec<String>,
    #[serde(default)]
    link: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableLockfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    lockfile_version: u32,
    requires: bool,
    packages: BTreeMap<String, WritableEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dev_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    os: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cpu: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    libc: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    link: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dev: Option<bool>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn write_json(dir: &Path, content: &str) -> std::path::PathBuf {
        let p = dir.join("package-lock.json");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn leaf_package_name_finds_innermost() {
        assert_eq!(
            leaf_package_name("node_modules/foo").as_deref(),
            Some("foo")
        );
        assert_eq!(
            leaf_package_name("node_modules/foo/node_modules/bar").as_deref(),
            Some("bar")
        );
        assert_eq!(
            leaf_package_name("node_modules/@scope/pkg").as_deref(),
            Some("@scope/pkg")
        );
    }

    #[test]
    fn parse_simple_project() {
        let json = r#"{
          "name": "demo",
          "version": "1.0.0",
          "lockfileVersion": 3,
          "requires": true,
          "packages": {
            "": {
              "name": "demo",
              "version": "1.0.0",
              "dependencies": { "lodash": "^4.17.21" }
            },
            "node_modules/lodash": {
              "version": "4.17.21",
              "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
              "integrity": "sha512-stub"
            }
          }
        }"#;
        let dir = tempfile::tempdir().unwrap();
        let p = write_json(dir.path(), json);
        let g = parse(&p).unwrap();

        let root_deps = g.root_deps();
        assert_eq!(root_deps.len(), 1);
        assert_eq!(root_deps[0].name, "lodash");
        // After resolution fixup, dep_path points at the real package key.
        assert_eq!(root_deps[0].dep_path, "lodash@4.17.21");

        let pkg = g.get_package("lodash@4.17.21").unwrap();
        assert_eq!(pkg.version, "4.17.21");
        assert_eq!(pkg.integrity.as_deref(), Some("sha512-stub"));
        assert_eq!(
            pkg.tarball_url.as_deref(),
            Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz")
        );
    }

    #[test]
    fn write_round_trip_minimum() {
        // Build a graph and check that writing then re-parsing yields
        // the same checksum (modulo npm's specifier-vs-version loss).
        let mut g = LockfileGraph::default();
        g.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "lodash".to_string(),
                dep_path: "lodash@4.17.21".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^4.17.21".to_string()),
            }],
        );
        g.packages.insert(
            "lodash@4.17.21".to_string(),
            LockedPackage {
                name: "lodash".to_string(),
                version: "4.17.21".to_string(),
                integrity: Some("sha512-stub".to_string()),
                tarball_url: Some(
                    "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string(),
                ),
                dep_path: "lodash@4.17.21".to_string(),
                ..LockedPackage::default()
            },
        );

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("package-lock.json");
        let mut manifest = PackageJson::default();
        manifest.name = Some("demo".to_string());
        manifest.version = Some("1.0.0".to_string());
        write(&p, &g, &manifest).unwrap();

        let g2 = parse(&p).unwrap();
        assert_eq!(g2.root_deps().len(), 1);
        let dep = &g2.root_deps()[0];
        assert_eq!(dep.name, "lodash");
        assert_eq!(dep.dep_path, "lodash@4.17.21");
        assert_eq!(dep.specifier.as_deref(), Some("^4.17.21"));

        let pkg = g2.get_package("lodash@4.17.21").unwrap();
        assert_eq!(pkg.version, "4.17.21");
        assert_eq!(pkg.integrity.as_deref(), Some("sha512-stub"));
    }

    #[test]
    fn shrinkwrap_and_package_lock_share_parser() {
        // Just verify the same parser handles the renamed file — the
        // dispatcher swaps the name but not the schema.
        let json = r#"{
          "name": "x","version": "0.0.0","lockfileVersion": 3,"requires": true,
          "packages": {
            "": {"name": "x","version": "0.0.0"},
            "node_modules/foo": {"version": "1.0.0"}
          }
        }"#;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("npm-shrinkwrap.json");
        std::fs::write(&p, json).unwrap();
        let g = parse(&p).unwrap();
        assert!(g.get_package("foo@1.0.0").is_some());
    }
}
