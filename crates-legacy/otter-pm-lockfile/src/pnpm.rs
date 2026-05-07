//! `pnpm-lock.yaml` v9 adapter.
//!
//! # Scope
//!
//! Round-trip-friendly implementation covering the 80% case:
//! - `settings:` (auto-install-peers, exclude-links-from-lockfile,
//!   lockfile-include-tarball-url)
//! - `overrides:` (flat string map)
//! - `catalogs:` (`{specifier, version}` entries)
//! - `time:` (name@version → ISO 8601)
//! - `ignoredOptionalDependencies:`
//! - `importers:` with `dependencies` / `devDependencies` /
//!   `optionalDependencies` blocks — each entry carries
//!   `{specifier, version}`
//! - `packages:` — resolution (integrity, tarball, directory,
//!   git commit+repo), `peerDependencies`, `peerDependenciesMeta`,
//!   `os` / `cpu` / `libc`
//! - `snapshots:` — per-dep-path `dependencies` /
//!   `optionalDependencies` / `bundledDependencies`
//!
//! # What's lossy on round-trip
//!
//! - Peer-context dep_path suffixes (e.g. `react@18.2.0(react-dom@18.2.0)`)
//!   are preserved verbatim as strings. We don't interpret them; the
//!   resolver is responsible for re-generating them during a fresh
//!   resolve, but a parse → write cycle of an unchanged lockfile
//!   keeps them byte-identical.
//! - Any fields beyond the list above are dropped on write. pnpm v9's
//!   schema is settled so this is rare in practice.
//!
//! # Wire format reference
//!
//! pnpm v9 lockfile spec lives in pnpm's source tree at
//! `packages/lockfile.types/src/Lockfile.ts`. We don't try to match
//! every schema variation — just the one pnpm emits by default.

use crate::{
    CatalogEntry, DepType, DirectDep, Error, LocalSource, LockedPackage, LockfileGraph,
    LockfileSettings, PeerDepMeta,
};
use otter_pm_manifest::PackageJson;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Current `lockfileVersion` we emit. Reading is tolerant of older
/// schemas that round-trip through the same structural shape.
const LOCKFILE_VERSION: &str = "9.0";

// ---------------------------------------------------------------------------
// Parse
// ---------------------------------------------------------------------------

pub fn parse(path: &Path) -> Result<LockfileGraph, Error> {
    let bytes = std::fs::read(path).map_err(|e| Error::Io(path.to_path_buf(), e.to_string()))?;
    let raw: RawLockfile = serde_yaml::from_slice(&bytes)
        .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;

    let settings = raw.settings.map(LockfileSettings::from).unwrap_or_default();
    let overrides: BTreeMap<String, String> = raw.overrides.unwrap_or_default();
    let ignored_optional_dependencies: BTreeSet<String> = raw
        .ignored_optional_dependencies
        .unwrap_or_default()
        .into_iter()
        .collect();
    let times: BTreeMap<String, String> = raw.time.unwrap_or_default();

    let catalogs: BTreeMap<String, BTreeMap<String, CatalogEntry>> = raw
        .catalogs
        .unwrap_or_default()
        .into_iter()
        .map(|(cat_name, entries)| {
            let entries = entries
                .into_iter()
                .map(|(pkg, e)| {
                    (
                        pkg,
                        CatalogEntry {
                            specifier: e.specifier,
                            version: e.version,
                        },
                    )
                })
                .collect();
            (cat_name, entries)
        })
        .collect();

    // Importers + any local-source packages they introduce (file: / link: / git:)
    // produce synthetic LockedPackage entries so the main graph has
    // one coherent source of truth.
    let mut importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();
    let mut local_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();

    for (importer_path, imp) in raw.importers.unwrap_or_default() {
        let mut deps: Vec<DirectDep> = Vec::new();
        push_importer_block(
            &mut deps,
            &mut local_packages,
            imp.dependencies.as_ref(),
            DepType::Production,
        );
        push_importer_block(
            &mut deps,
            &mut local_packages,
            imp.dev_dependencies.as_ref(),
            DepType::Dev,
        );
        push_importer_block(
            &mut deps,
            &mut local_packages,
            imp.optional_dependencies.as_ref(),
            DepType::Optional,
        );
        importers.insert(importer_path, deps);
    }

    // Registry packages: keyed by dep_path (possibly with peer suffix).
    let mut packages: BTreeMap<String, LockedPackage> = local_packages;
    let raw_packages = raw.packages.unwrap_or_default();
    let raw_snapshots = raw.snapshots.unwrap_or_default();

    // `packages:` carries the per-version metadata (resolution, peers,
    // platform constraints). `snapshots:` carries the per-peer-context
    // edge list. Join them into a single `LockedPackage`.
    //
    // Keys in `packages:` are `name@version` (no peer suffix). Keys in
    // `snapshots:` are `name@version[(peer@context)...]`. We emit one
    // `LockedPackage` per snapshot, sharing base metadata from the
    // matching `packages:` entry.
    for (dep_path, snapshot) in &raw_snapshots {
        let base_key = strip_peer_suffix(dep_path);
        let pkg_info = raw_packages.get(base_key);
        let (name, version) = match parse_dep_path(base_key) {
            Some(v) => v,
            None => continue,
        };
        let (integrity, tarball_url, local_source) = decode_resolution(
            pkg_info.and_then(|p| p.resolution.as_ref()),
            &name,
            &version,
        );

        let locked = LockedPackage {
            name: name.clone(),
            version,
            integrity,
            dependencies: snapshot.dependencies.clone().unwrap_or_default(),
            optional_dependencies: snapshot.optional_dependencies.clone().unwrap_or_default(),
            peer_dependencies: pkg_info
                .and_then(|p| p.peer_dependencies.clone())
                .unwrap_or_default(),
            peer_dependencies_meta: pkg_info
                .and_then(|p| p.peer_dependencies_meta.clone())
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        PeerDepMeta {
                            optional: v.optional,
                        },
                    )
                })
                .collect(),
            dep_path: dep_path.clone(),
            local_source,
            os: pkg_info.map(|p| p.os.clone()).unwrap_or_default(),
            cpu: pkg_info.map(|p| p.cpu.clone()).unwrap_or_default(),
            libc: pkg_info.map(|p| p.libc.clone()).unwrap_or_default(),
            bundled_dependencies: snapshot.bundled_dependencies.clone().unwrap_or_default(),
            tarball_url,
            alias_of: pkg_info.and_then(|p| p.alias_of.clone()),
            yarn_checksum: None,
        };
        packages.insert(dep_path.clone(), locked);
    }

    // Any package listed in `packages:` but missing from `snapshots:`
    // still ends up in the graph (edge case — freshly published
    // versions not yet linked anywhere). Empty `dependencies` is fine.
    for (base_key, pkg_info) in &raw_packages {
        if packages.contains_key(base_key) {
            continue;
        }
        let Some((name, version)) = parse_dep_path(base_key) else {
            continue;
        };
        let (integrity, tarball_url, local_source) =
            decode_resolution(pkg_info.resolution.as_ref(), &name, &version);
        packages.insert(
            base_key.clone(),
            LockedPackage {
                name,
                version,
                integrity,
                peer_dependencies: pkg_info.peer_dependencies.clone().unwrap_or_default(),
                peer_dependencies_meta: pkg_info
                    .peer_dependencies_meta
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(k, v)| {
                        (
                            k,
                            PeerDepMeta {
                                optional: v.optional,
                            },
                        )
                    })
                    .collect(),
                dep_path: base_key.clone(),
                local_source,
                os: pkg_info.os.clone(),
                cpu: pkg_info.cpu.clone(),
                libc: pkg_info.libc.clone(),
                tarball_url,
                alias_of: pkg_info.alias_of.clone(),
                ..LockedPackage::default()
            },
        );
    }

    Ok(LockfileGraph {
        importers,
        packages,
        settings,
        overrides,
        ignored_optional_dependencies,
        times,
        skipped_optional_dependencies: BTreeMap::new(),
        catalogs,
    })
}

fn push_importer_block(
    out: &mut Vec<DirectDep>,
    local_packages: &mut BTreeMap<String, LockedPackage>,
    block: Option<&BTreeMap<String, RawDepSpec>>,
    dep_type: DepType,
) {
    let Some(block) = block else { return };
    for (name, spec) in block {
        // `version:` may carry a local source (`link:../foo`, `file:./x.tgz`,
        // `git+…#<sha>`, `https://.../pkg.tgz`). In that case we synthesize a
        // LockedPackage so the graph stays self-contained.
        let dep_path = if let Some(local) = parse_local_source(&spec.version) {
            let dp = local.dep_path(name);
            local_packages
                .entry(dp.clone())
                .or_insert_with(|| LockedPackage {
                    name: name.clone(),
                    version: "0.0.0".to_string(),
                    dep_path: dp.clone(),
                    local_source: Some(local),
                    ..LockedPackage::default()
                });
            dp
        } else {
            format!("{name}@{}", spec.version)
        };
        out.push(DirectDep {
            name: name.clone(),
            dep_path,
            dep_type,
            specifier: Some(spec.specifier.clone()),
        });
    }
}

/// Parse a pnpm `version:` field that might carry a `file:` / `link:` /
/// `git+…` / `https://` local source. Returns `None` for plain semver.
fn parse_local_source(spec: &str) -> Option<LocalSource> {
    if let Some(rest) = spec.strip_prefix("file:") {
        let path = std::path::PathBuf::from(rest);
        if rest.ends_with(".tgz") || rest.ends_with(".tar.gz") {
            return Some(LocalSource::Tarball(path));
        }
        return Some(LocalSource::Directory(path));
    }
    if let Some(rest) = spec.strip_prefix("link:") {
        return Some(LocalSource::Link(std::path::PathBuf::from(rest)));
    }
    if spec.starts_with("git+")
        || spec.starts_with("git://")
        || (spec.starts_with("https://") && spec.ends_with(".git"))
    {
        let (url, committish) = match spec.rsplit_once('#') {
            Some((u, c)) => (
                u.strip_prefix("git+").unwrap_or(u).to_string(),
                Some(c.to_string()),
            ),
            None => (spec.strip_prefix("git+").unwrap_or(spec).to_string(), None),
        };
        return Some(LocalSource::Git(crate::GitSource {
            url,
            committish: committish.clone(),
            resolved: committish.unwrap_or_default(),
        }));
    }
    if (spec.starts_with("https://") || spec.starts_with("http://"))
        && (spec.ends_with(".tgz") || spec.ends_with(".tar.gz"))
    {
        return Some(LocalSource::RemoteTarball(crate::RemoteTarballSource {
            url: spec.to_string(),
            integrity: String::new(),
        }));
    }
    None
}

fn decode_resolution(
    res: Option<&Resolution>,
    _name: &str,
    _version: &str,
) -> (Option<String>, Option<String>, Option<LocalSource>) {
    let Some(res) = res else {
        return (None, None, None);
    };
    // git dep: `resolution: {commit: <sha>, repo: <url>, type: "git"}`
    if let (Some(commit), Some(repo)) = (&res.commit, &res.repo) {
        return (
            None,
            None,
            Some(LocalSource::Git(crate::GitSource {
                url: repo.clone(),
                committish: None,
                resolved: commit.clone(),
            })),
        );
    }
    // file:/link: dep: `resolution: {directory: <path>, type: "directory"}`
    if let Some(dir) = &res.directory {
        return (
            None,
            None,
            Some(LocalSource::Directory(std::path::PathBuf::from(dir))),
        );
    }
    // Remote tarball (registry or direct URL): `resolution: {integrity,
    // tarball?}`. When `lockfile-include-tarball-url: true`, `tarball`
    // carries the full URL; otherwise we only preserve integrity and
    // the installer derives the URL at fetch time from `.npmrc`.
    (res.integrity.clone(), res.tarball.clone(), None)
}

/// Strip a peer-context suffix from a dep_path: `react@18.0.0(react-dom@18.0.0)` → `react@18.0.0`.
/// Unsuffixed keys pass through unchanged.
fn strip_peer_suffix(dep_path: &str) -> &str {
    match dep_path.find('(') {
        Some(i) => &dep_path[..i],
        None => dep_path,
    }
}

/// Parse `<name>@<version>` (scoped names supported).
/// Returns None on malformed input.
pub(crate) fn parse_dep_path(dep_path: &str) -> Option<(String, String)> {
    // pnpm v6-v8 wrote a leading `/`; tolerate it on read.
    let s = dep_path.strip_prefix('/').unwrap_or(dep_path);
    let at = if s.starts_with('@') {
        let slash = s.find('/')? + 1;
        slash + s[slash..].find('@')?
    } else {
        s.find('@')?
    };
    Some((s[..at].to_string(), s[at + 1..].to_string()))
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

pub fn write(path: &Path, graph: &LockfileGraph, _manifest: &PackageJson) -> Result<(), Error> {
    let settings = WritableSettings::from(&graph.settings);

    let overrides = (!graph.overrides.is_empty()).then(|| graph.overrides.clone());
    let ignored_optional_dependencies =
        (!graph.ignored_optional_dependencies.is_empty()).then(|| {
            graph
                .ignored_optional_dependencies
                .iter()
                .cloned()
                .collect()
        });
    let time = (!graph.times.is_empty()).then(|| graph.times.clone());
    let catalogs = (!graph.catalogs.is_empty()).then(|| {
        graph
            .catalogs
            .iter()
            .map(|(name, entries)| {
                (
                    name.clone(),
                    entries
                        .iter()
                        .map(|(pkg, e)| {
                            (
                                pkg.clone(),
                                WritableCatalogEntry {
                                    specifier: e.specifier.clone(),
                                    version: e.version.clone(),
                                },
                            )
                        })
                        .collect(),
                )
            })
            .collect()
    });

    let importers: BTreeMap<String, WritableImporter> = graph
        .importers
        .iter()
        .map(|(k, deps)| (k.clone(), writable_importer(deps, &graph.packages)))
        .collect();

    // Split into `packages:` (name@version → metadata) and
    // `snapshots:` (dep_path → deps edges).
    let mut packages: BTreeMap<String, WritablePackageInfo> = BTreeMap::new();
    let mut snapshots: BTreeMap<String, WritableSnapshot> = BTreeMap::new();
    for (dep_path, pkg) in &graph.packages {
        let base_key = format!("{}@{}", pkg.name, pkg.version);
        packages
            .entry(base_key)
            .or_insert_with(|| writable_package_info(pkg));
        snapshots.insert(dep_path.clone(), writable_snapshot(pkg));
    }

    let out = WritableLockfile {
        lockfile_version: LOCKFILE_VERSION.to_string(),
        settings,
        overrides,
        catalogs,
        ignored_optional_dependencies,
        time,
        importers,
        packages,
        snapshots,
    };
    let mut yaml =
        serde_yaml::to_string(&out).map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    std::fs::write(path, yaml).map_err(|e| Error::Io(path.to_path_buf(), e.to_string()))
}

fn writable_importer(
    deps: &[DirectDep],
    packages: &BTreeMap<String, LockedPackage>,
) -> WritableImporter {
    let mut dependencies: BTreeMap<String, WritableDepSpec> = BTreeMap::new();
    let mut dev_dependencies: BTreeMap<String, WritableDepSpec> = BTreeMap::new();
    let mut optional_dependencies: BTreeMap<String, WritableDepSpec> = BTreeMap::new();
    for d in deps {
        let version = match packages.get(&d.dep_path) {
            Some(p) => match &p.local_source {
                Some(src) => match src {
                    LocalSource::Directory(path) | LocalSource::Tarball(path) => {
                        format!("file:{}", path.to_string_lossy().replace('\\', "/"))
                    }
                    LocalSource::Link(path) => {
                        format!("link:{}", path.to_string_lossy().replace('\\', "/"))
                    }
                    LocalSource::Git(g) => {
                        if g.resolved.is_empty() {
                            g.url.clone()
                        } else {
                            format!("{}#{}", g.url, g.resolved)
                        }
                    }
                    LocalSource::RemoteTarball(t) => t.url.clone(),
                },
                None => p.version.clone(),
            },
            None => {
                // Lockfile references a package we don't have metadata
                // for — fall back to the dep_path's version segment.
                parse_dep_path(&d.dep_path)
                    .map(|(_, v)| v)
                    .unwrap_or_default()
            }
        };
        let spec = WritableDepSpec {
            specifier: d.specifier.clone().unwrap_or_default(),
            version,
        };
        match d.dep_type {
            DepType::Production => {
                dependencies.insert(d.name.clone(), spec);
            }
            DepType::Dev => {
                dev_dependencies.insert(d.name.clone(), spec);
            }
            DepType::Optional => {
                optional_dependencies.insert(d.name.clone(), spec);
            }
        }
    }
    WritableImporter {
        dependencies: (!dependencies.is_empty()).then_some(dependencies),
        dev_dependencies: (!dev_dependencies.is_empty()).then_some(dev_dependencies),
        optional_dependencies: (!optional_dependencies.is_empty()).then_some(optional_dependencies),
    }
}

fn writable_package_info(pkg: &LockedPackage) -> WritablePackageInfo {
    let resolution = match &pkg.local_source {
        Some(LocalSource::Git(g)) => Some(WritableResolution {
            integrity: None,
            tarball: None,
            directory: None,
            commit: Some(g.resolved.clone()),
            repo: Some(g.url.clone()),
            type_: Some("git".to_string()),
        }),
        Some(LocalSource::Directory(p) | LocalSource::Tarball(p)) => Some(WritableResolution {
            integrity: None,
            tarball: None,
            directory: Some(p.to_string_lossy().replace('\\', "/")),
            commit: None,
            repo: None,
            type_: Some("directory".to_string()),
        }),
        Some(LocalSource::Link(_)) => None,
        Some(LocalSource::RemoteTarball(t)) => Some(WritableResolution {
            integrity: Some(t.integrity.clone()).filter(|s| !s.is_empty()),
            tarball: Some(t.url.clone()),
            directory: None,
            commit: None,
            repo: None,
            type_: None,
        }),
        None => pkg.integrity.as_ref().map(|integrity| WritableResolution {
            integrity: Some(integrity.clone()),
            tarball: pkg.tarball_url.clone(),
            directory: None,
            commit: None,
            repo: None,
            type_: None,
        }),
    };
    WritablePackageInfo {
        resolution,
        peer_dependencies: (!pkg.peer_dependencies.is_empty())
            .then(|| pkg.peer_dependencies.clone()),
        peer_dependencies_meta: (!pkg.peer_dependencies_meta.is_empty()).then(|| {
            pkg.peer_dependencies_meta
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        RawPeerDepMeta {
                            optional: v.optional,
                        },
                    )
                })
                .collect()
        }),
        os: pkg.os.clone(),
        cpu: pkg.cpu.clone(),
        libc: pkg.libc.clone(),
        alias_of: pkg.alias_of.clone(),
    }
}

fn writable_snapshot(pkg: &LockedPackage) -> WritableSnapshot {
    WritableSnapshot {
        dependencies: (!pkg.dependencies.is_empty()).then(|| pkg.dependencies.clone()),
        optional_dependencies: (!pkg.optional_dependencies.is_empty())
            .then(|| pkg.optional_dependencies.clone()),
        bundled_dependencies: (!pkg.bundled_dependencies.is_empty())
            .then(|| pkg.bundled_dependencies.clone()),
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

// -- Read-side (permissive) --

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLockfile {
    #[serde(default)]
    #[allow(dead_code)]
    lockfile_version: serde_yaml::Value,
    #[serde(default)]
    settings: Option<RawSettings>,
    #[serde(default)]
    overrides: Option<BTreeMap<String, String>>,
    #[serde(default)]
    catalogs: Option<BTreeMap<String, BTreeMap<String, RawCatalogEntry>>>,
    #[serde(default)]
    ignored_optional_dependencies: Option<Vec<String>>,
    #[serde(default)]
    time: Option<BTreeMap<String, String>>,
    #[serde(default)]
    importers: Option<BTreeMap<String, RawImporter>>,
    #[serde(default)]
    packages: Option<BTreeMap<String, RawPackageInfo>>,
    #[serde(default)]
    snapshots: Option<BTreeMap<String, RawSnapshot>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSettings {
    #[serde(default)]
    auto_install_peers: Option<bool>,
    #[serde(default)]
    exclude_links_from_lockfile: Option<bool>,
    #[serde(default)]
    lockfile_include_tarball_url: Option<bool>,
}

impl From<RawSettings> for LockfileSettings {
    fn from(r: RawSettings) -> Self {
        let d = LockfileSettings::default();
        Self {
            auto_install_peers: r.auto_install_peers.unwrap_or(d.auto_install_peers),
            exclude_links_from_lockfile: r
                .exclude_links_from_lockfile
                .unwrap_or(d.exclude_links_from_lockfile),
            lockfile_include_tarball_url: r
                .lockfile_include_tarball_url
                .unwrap_or(d.lockfile_include_tarball_url),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawImporter {
    dependencies: Option<BTreeMap<String, RawDepSpec>>,
    dev_dependencies: Option<BTreeMap<String, RawDepSpec>>,
    optional_dependencies: Option<BTreeMap<String, RawDepSpec>>,
}

#[derive(Debug, Deserialize)]
struct RawDepSpec {
    specifier: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct RawCatalogEntry {
    specifier: String,
    version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPackageInfo {
    resolution: Option<Resolution>,
    peer_dependencies: Option<BTreeMap<String, String>>,
    peer_dependencies_meta: Option<BTreeMap<String, RawPeerDepMeta>>,
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    cpu: Vec<String>,
    #[serde(default)]
    libc: Vec<String>,
    #[serde(default)]
    alias_of: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RawPeerDepMeta {
    #[serde(default)]
    optional: bool,
}

#[derive(Debug, Deserialize)]
struct Resolution {
    integrity: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    tarball: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    type_: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSnapshot {
    #[serde(default)]
    dependencies: Option<BTreeMap<String, String>>,
    #[serde(default)]
    optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(default)]
    bundled_dependencies: Option<Vec<String>>,
}

// -- Write-side --

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableLockfile {
    lockfile_version: String,
    settings: WritableSettings,
    #[serde(skip_serializing_if = "Option::is_none")]
    overrides: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalogs: Option<BTreeMap<String, BTreeMap<String, WritableCatalogEntry>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ignored_optional_dependencies: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    time: Option<BTreeMap<String, String>>,
    importers: BTreeMap<String, WritableImporter>,
    packages: BTreeMap<String, WritablePackageInfo>,
    snapshots: BTreeMap<String, WritableSnapshot>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableSettings {
    auto_install_peers: bool,
    exclude_links_from_lockfile: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    lockfile_include_tarball_url: bool,
}

impl From<&LockfileSettings> for WritableSettings {
    fn from(s: &LockfileSettings) -> Self {
        Self {
            auto_install_peers: s.auto_install_peers,
            exclude_links_from_lockfile: s.exclude_links_from_lockfile,
            lockfile_include_tarball_url: s.lockfile_include_tarball_url,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableImporter {
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dev_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
}

#[derive(Debug, Serialize)]
struct WritableDepSpec {
    specifier: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct WritableCatalogEntry {
    specifier: String,
    version: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePackageInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<WritableResolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_dependencies_meta: Option<BTreeMap<String, RawPeerDepMeta>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    os: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cpu: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    libc: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alias_of: Option<String>,
}

#[derive(Debug, Serialize)]
struct WritableResolution {
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tarball: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    type_: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundled_dependencies: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn write_and_read(yaml: &str) -> LockfileGraph {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pnpm-lock.yaml");
        std::fs::write(&p, yaml).unwrap();
        parse(&p).unwrap()
    }

    #[test]
    fn parse_dep_path_plain_and_scoped() {
        assert_eq!(
            parse_dep_path("lodash@4.17.21"),
            Some(("lodash".to_string(), "4.17.21".to_string()))
        );
        assert_eq!(
            parse_dep_path("@babel/core@7.24.0"),
            Some(("@babel/core".to_string(), "7.24.0".to_string()))
        );
        assert_eq!(
            parse_dep_path("/legacy@1.0.0"),
            Some(("legacy".to_string(), "1.0.0".to_string()))
        );
    }

    #[test]
    fn strip_peer_suffix_basic() {
        assert_eq!(strip_peer_suffix("react@18.0.0"), "react@18.0.0");
        assert_eq!(
            strip_peer_suffix("react@18.0.0(react-dom@18.0.0)"),
            "react@18.0.0"
        );
    }

    #[test]
    fn minimal_round_trip_single_importer() {
        let yaml = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies:
      lodash:
        specifier: ^4.17.21
        version: 4.17.21

packages:
  lodash@4.17.21:
    resolution:
      integrity: sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg==

snapshots:
  lodash@4.17.21: {}
"#;
        let graph = write_and_read(yaml);
        assert_eq!(graph.root_deps().len(), 1);
        let dep = &graph.root_deps()[0];
        assert_eq!(dep.name, "lodash");
        assert_eq!(dep.dep_type, DepType::Production);
        assert_eq!(dep.specifier.as_deref(), Some("^4.17.21"));

        let pkg = graph.get_package("lodash@4.17.21").unwrap();
        assert_eq!(pkg.version, "4.17.21");
        assert!(pkg.integrity.as_deref().unwrap().starts_with("sha512-"));
    }

    #[test]
    fn round_trip_preserves_checksum() {
        let yaml = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies:
      is-odd:
        specifier: ^3
        version: 3.0.1
    devDependencies:
      typescript:
        specifier: ^5
        version: 5.0.0

packages:
  is-odd@3.0.1:
    resolution: {integrity: sha512-stub}
  typescript@5.0.0:
    resolution: {integrity: sha512-tsx}

snapshots:
  is-odd@3.0.1: {}
  typescript@5.0.0: {}
"#;
        let g1 = write_and_read(yaml);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pnpm-lock.yaml");
        write(&p, &g1, &PackageJson::default()).unwrap();
        let g2 = parse(&p).unwrap();
        assert_eq!(g1.checksum(), g2.checksum());
    }

    #[test]
    fn preserves_overrides_and_catalogs_and_time() {
        let yaml = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

overrides:
  foo: ^1.0.0

catalogs:
  default:
    react:
      specifier: ^18
      version: 18.2.0

time:
  lodash@4.17.21: '2021-02-20T00:00:00.000Z'

importers:
  .: {}

packages: {}
snapshots: {}
"#;
        let g = write_and_read(yaml);
        assert_eq!(g.overrides.get("foo").map(String::as_str), Some("^1.0.0"));
        assert!(g.catalogs.contains_key("default"));
        let react = &g.catalogs["default"]["react"];
        assert_eq!(react.specifier, "^18");
        assert_eq!(react.version, "18.2.0");
        assert_eq!(
            g.times.get("lodash@4.17.21").map(String::as_str),
            Some("2021-02-20T00:00:00.000Z")
        );
    }

    #[test]
    fn workspace_importers_are_preserved() {
        let yaml = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies: {}
  packages/ui:
    dependencies:
      react:
        specifier: ^18
        version: 18.2.0
  apps/web:
    dependencies:
      ui:
        specifier: workspace:*
        version: link:../../packages/ui

packages:
  react@18.2.0:
    resolution: {integrity: sha512-react}

snapshots:
  react@18.2.0: {}
"#;
        let g = write_and_read(yaml);
        assert_eq!(g.importers.len(), 3);
        assert!(g.importers.contains_key("."));
        assert!(g.importers.contains_key("packages/ui"));
        assert!(g.importers.contains_key("apps/web"));
        // `link:` dep got a synthesized package entry.
        let web_deps = &g.importers["apps/web"];
        assert_eq!(web_deps.len(), 1);
        let locked = g.get_package(&web_deps[0].dep_path).unwrap();
        assert!(matches!(locked.local_source, Some(LocalSource::Link(_))));
    }

    #[test]
    fn peer_context_dep_path_is_preserved() {
        let yaml = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies:
      styled-components:
        specifier: ^6
        version: 6.1.0(react@18.2.0)

packages:
  styled-components@6.1.0:
    resolution: {integrity: sha512-sc}
    peerDependencies:
      react: ^18
  react@18.2.0:
    resolution: {integrity: sha512-react}

snapshots:
  styled-components@6.1.0(react@18.2.0):
    dependencies:
      react: 18.2.0
  react@18.2.0: {}
"#;
        let g = write_and_read(yaml);
        assert!(
            g.packages
                .contains_key("styled-components@6.1.0(react@18.2.0)")
        );
        assert!(g.packages.contains_key("react@18.2.0"));
    }
}
