//! Registry dependency resolution: every dependency edge a project's graph
//! names on the registry, resolved to one exact package version.
//!
//! Resolution walks the graph breadth-first in a fixed order, so the same
//! manifests always give the same graph. The manifests one wave of edges
//! needs are fetched together, up to [`MANIFEST_CONCURRENCY`] requests at
//! once, before any edge of the wave resolves; what an edge resolves to never
//! depends on which response arrived first.
//!
//! # Contents
//! - [`enrich_resolution_with_registry_metadata`] — resolve the graph's open
//!   registry edges and every edge the packages they reach declare.
//! - Version choice: a version already in the graph that the range admits,
//!   else the manifest's `latest` when the range admits it, else the newest
//!   admitted release, each through the release-age gate.
//!
//! # Invariants
//! - A resolved registry package's id names its exact version
//!   (`name@npm:1.2.3`): one package version is one graph node however many
//!   ranges reach it. An `npm:` alias installs the package it names under the
//!   alias (`alias@npm:package@1.2.3`).
//! - A registry package's own dev dependencies are never edges.
//! - Peer edges resolve after every other edge: a version already in the
//!   graph satisfies one, the newest the range admits; a peer the graph does
//!   not hold at all is installed. A peer its package marks optional is never
//!   installed.
//! - An optional edge that cannot resolve (no admitted version, a specifier
//!   no registry serves, a failed fetch, a package for another platform) is
//!   dropped with a warning; a required one fails the resolution.
//! - Edges to the unresolved placeholders the local graph starts with are
//!   rewritten to resolved packages, and placeholders nothing reaches any
//!   more leave the graph and the lockfile.
//!
//! # See also
//! - [`crate::NpmRange`] for range matching.
//! - [`crate::release_age`] for the cooling window.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use futures_util::stream::{self, StreamExt};
use otter_pm_lockfile::{
    LifecycleMetadata, LockedPackage, ResolvedSource, ResolvedSourceKind, TrustState,
};
use otter_pm_manifest::PackageManifest;

use crate::release_age::ReleaseAgeGate;
use crate::{
    FsRegistryMetadataCache, InstallPolicy, InstallWarning, LocalResolution, NpmPackageVersion,
    NpmRange, NpmRegistryMetadata, PackageDependencyKind, PackageId, PackageManagerError,
    PackageRoot, RegistryMetadataClient, infer_project_root, insert_bins_for_manifest,
    is_tarball_reference, package_name_path,
};

/// The most registry manifest requests one resolution keeps in flight.
pub(crate) const MANIFEST_CONCURRENCY: usize = 64;

/// One dependency edge still to resolve.
#[derive(Debug, Clone)]
struct Edge {
    from: PackageId,
    name: String,
    spec: String,
    kind: PackageDependencyKind,
    /// A peer edge the graph held no version for, now resolved like any
    /// other edge.
    peer_pass: bool,
}

/// What a dependency specifier asks the registry for.
enum Spec<'a> {
    /// A range or dist-tag of `package`, installed under the edge's name.
    Registry { package: &'a str, range: &'a str },
    /// A tarball URL, fetched as it is.
    Tarball,
    /// A git, GitHub, file or link specifier: no registry serves it.
    Unsupported,
}

fn classify<'a>(name: &'a str, spec: &'a str) -> Spec<'a> {
    let spec = spec.trim();
    if let Some(alias) = spec.strip_prefix("npm:") {
        // `npm:pkg@range`, `npm:@scope/pkg@range` or a bare `npm:pkg`.
        let (package, range) = match alias.rfind('@').filter(|&at| at > 0) {
            Some(at) => (&alias[..at], &alias[at + 1..]),
            None => (alias, "*"),
        };
        return Spec::Registry { package, range };
    }
    if is_tarball_reference(spec) {
        return Spec::Tarball;
    }
    let unsupported = spec.contains(':')
        || spec.starts_with(['.', '/'])
        // A GitHub `user/repo` shorthand.
        || (spec.contains('/') && !spec.starts_with('@'));
    if unsupported {
        Spec::Unsupported
    } else {
        Spec::Registry {
            package: name,
            range: if spec.is_empty() { "*" } else { spec },
        }
    }
}

/// The running platform in npm's `os` / `cpu` names.
fn npm_platform() -> (&'static str, &'static str) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let cpu = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        other => other,
    };
    (os, cpu)
}

/// npm's `os` / `cpu` list: `!name` excludes a platform, a plain name
/// admits only the platforms the list names.
fn platform_admits(list: &[String], current: &str) -> bool {
    if list
        .iter()
        .any(|entry| entry.strip_prefix('!') == Some(current))
    {
        return false;
    }
    let mut admitted = list
        .iter()
        .filter(|entry| !entry.starts_with('!'))
        .peekable();
    admitted.peek().is_none() || admitted.any(|entry| entry == current)
}

/// Resolve every open registry edge of `resolution` and the edges the
/// packages it reaches declare, fetching manifests through `cache`.
///
/// # Errors
/// A required edge that cannot resolve, a failed fetch of a manifest a
/// required edge needs, or a release-age refusal under a strict policy.
pub async fn enrich_resolution_with_registry_metadata(
    resolution: &mut LocalResolution,
    cache: &FsRegistryMetadataCache,
    client: &impl RegistryMetadataClient,
    policy: &InstallPolicy,
    warnings: &mut Vec<InstallWarning>,
) -> Result<(), PackageManagerError> {
    let mut resolver = Resolver {
        project_root: infer_project_root(&resolution.graph),
        gate: ReleaseAgeGate::new(policy, SystemTime::now()),
        manifests: HashMap::new(),
        versions: BTreeMap::new(),
        placeholders: BTreeSet::new(),
    };
    resolver.index(resolution);
    let mut queue = resolver.open_edges(resolution);
    let mut peers = Vec::new();
    loop {
        if queue.is_empty() {
            if peers.is_empty() {
                break;
            }
            for edge in std::mem::take(&mut peers) {
                if let Some(edge) = resolver.satisfy_peer(resolution, edge, warnings) {
                    queue.push_back(edge);
                }
            }
            continue;
        }
        let wave: Vec<Edge> = queue.drain(..).collect();
        resolver.fetch_manifests(&wave, cache, client).await;
        for edge in wave {
            if edge.kind == PackageDependencyKind::Peer && !edge.peer_pass {
                peers.push(edge);
                continue;
            }
            resolver.resolve(resolution, edge, &mut queue, &mut peers, warnings)?;
        }
    }
    resolver.remove_unreached_placeholders(resolution);
    Ok(())
}

struct Resolver {
    project_root: PathBuf,
    gate: ReleaseAgeGate,
    /// Fetched manifests by package name, failures included.
    manifests: HashMap<String, Result<NpmRegistryMetadata, PackageManagerError>>,
    /// Resolved versions of each package, by package name.
    versions: BTreeMap<String, BTreeMap<semver::Version, PackageId>>,
    /// The unresolved registry packages the local graph started with.
    placeholders: BTreeSet<PackageId>,
}

impl Resolver {
    /// Record the registry packages the graph already holds at one version,
    /// and the placeholders still to resolve.
    fn index(&mut self, resolution: &LocalResolution) {
        for (id, package) in &resolution.lockfile.packages {
            let Some(ResolvedSource {
                kind: ResolvedSourceKind::Registry,
                reference,
            }) = &package.resolved
            else {
                continue;
            };
            let id = PackageId::new(id.clone());
            if !is_tarball_reference(reference) {
                self.placeholders.insert(id);
            } else if id == PackageId::registry_version(&package.name, &package.version)
                && let Ok(version) = semver::Version::parse(&package.version)
            {
                self.versions
                    .entry(package.name.clone())
                    .or_default()
                    .insert(version, id);
            }
        }
    }

    /// Edges from any package to a placeholder, in graph order.
    fn open_edges(&self, resolution: &LocalResolution) -> VecDeque<Edge> {
        let mut edges = VecDeque::new();
        for (from, dependencies) in &resolution.graph.dependencies {
            for (name, target) in dependencies {
                if !self.placeholders.contains(target) {
                    continue;
                }
                let Some(spec) = resolution
                    .lockfile
                    .packages
                    .get(target.as_str())
                    .and_then(|package| package.resolved.as_ref())
                    .map(|resolved| resolved.reference.clone())
                else {
                    continue;
                };
                edges.push_back(Edge {
                    from: from.clone(),
                    name: name.clone(),
                    spec,
                    kind: resolution
                        .graph
                        .dependency_kind(from, name)
                        .unwrap_or(PackageDependencyKind::Runtime),
                    peer_pass: false,
                });
            }
        }
        edges
    }

    /// Fetch, all at once, the manifests `wave` needs that are not in hand.
    async fn fetch_manifests(
        &mut self,
        wave: &[Edge],
        cache: &FsRegistryMetadataCache,
        client: &impl RegistryMetadataClient,
    ) {
        let names: BTreeSet<String> = wave
            .iter()
            .filter_map(|edge| match classify(&edge.name, &edge.spec) {
                Spec::Registry { package, .. } => Some(package.to_string()),
                Spec::Tarball | Spec::Unsupported => None,
            })
            .filter(|name| !self.manifests.contains_key(name))
            .collect();
        let fetched: Vec<_> = stream::iter(names)
            .map(|name| async move {
                let metadata = cache.get_or_fetch(&name, client).await;
                (name, metadata)
            })
            .buffer_unordered(MANIFEST_CONCURRENCY)
            .collect()
            .await;
        self.manifests.extend(fetched);
    }

    /// Satisfy `edge` from the versions the graph holds; the edge to resolve
    /// from the registry when it holds none.
    fn satisfy_peer(
        &self,
        resolution: &mut LocalResolution,
        edge: Edge,
        warnings: &mut Vec<InstallWarning>,
    ) -> Option<Edge> {
        let package = match classify(&edge.name, &edge.spec) {
            Spec::Registry { package, .. } => package.to_string(),
            Spec::Tarball | Spec::Unsupported => {
                drop_edge(resolution, &edge);
                return None;
            }
        };
        let Some(held) = self.versions.get(&package).filter(|held| !held.is_empty()) else {
            // Nothing provides it: install it as the package declares it.
            return Some(Edge {
                peer_pass: true,
                ..edge
            });
        };
        let range = match classify(&edge.name, &edge.spec) {
            Spec::Registry { range, .. } => NpmRange::parse(range),
            _ => None,
        };
        let admitted = held
            .iter()
            .rev()
            .find(|(version, _)| range.as_ref().is_none_or(|range| range.matches(version)));
        let (_, target) = admitted.unwrap_or_else(|| {
            let (version, target) = held.iter().next_back().expect("a held version");
            warnings.push(InstallWarning::new(
                "PM_UNMET_PEER",
                format!(
                    "`{}` wants peer `{}@{}`; the graph holds {version}",
                    edge.from, edge.name, edge.spec
                ),
            ));
            (version, target)
        });
        link(resolution, &edge, target);
        None
    }

    fn resolve(
        &mut self,
        resolution: &mut LocalResolution,
        edge: Edge,
        queue: &mut VecDeque<Edge>,
        peers: &mut Vec<Edge>,
        warnings: &mut Vec<InstallWarning>,
    ) -> Result<(), PackageManagerError> {
        let optional = matches!(
            edge.kind,
            PackageDependencyKind::Optional | PackageDependencyKind::Peer
        );
        let (package, range) = match classify(&edge.name, &edge.spec) {
            Spec::Registry { package, range } => (package.to_string(), range.to_string()),
            Spec::Tarball => {
                let id = PackageId::tarball(&edge.name, &edge.spec);
                crate::ensure_tarball_package(
                    &self.project_root,
                    &mut resolution.graph,
                    &mut resolution.lockfile,
                    &id,
                    &edge.name,
                    &edge.spec,
                );
                link(resolution, &edge, &id);
                return Ok(());
            }
            Spec::Unsupported => {
                if optional {
                    warnings.push(InstallWarning::new(
                        "PM_DEPENDENCY_SKIPPED",
                        format!(
                            "`{}` names `{}` as `{}`, which no registry serves; it is left out",
                            edge.from, edge.name, edge.spec
                        ),
                    ));
                    drop_edge(resolution, &edge);
                    return Ok(());
                }
                return Err(PackageManagerError::NoMatchingVersion {
                    package: edge.name.clone(),
                    range: edge.spec.clone(),
                });
            }
        };
        let aliased = package != edge.name;
        // A version the graph already holds satisfies the range.
        if !aliased
            && let Some(parsed) = NpmRange::parse(&range)
            && let Some((_, target)) = self.versions.get(&package).and_then(|held| {
                held.iter()
                    .rev()
                    .find(|(version, _)| parsed.matches(version))
            })
        {
            let target = target.clone();
            link(resolution, &edge, &target);
            return Ok(());
        }
        let chosen = match self.manifests.get(&package) {
            Some(Ok(metadata)) => select_registry_version(metadata, &range, &self.gate, warnings),
            Some(Err(error)) => Err(error.clone()),
            None => Err(PackageManagerError::NoMatchingVersion {
                package: package.clone(),
                range: range.clone(),
            }),
        };
        let version = match chosen {
            Ok(version) => version,
            Err(error) if optional => {
                warnings.push(InstallWarning::new(
                    "PM_DEPENDENCY_SKIPPED",
                    format!(
                        "optional `{}@{}` is left out: {error}",
                        edge.name, edge.spec
                    ),
                ));
                drop_edge(resolution, &edge);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let (os, cpu) = npm_platform();
        if !platform_admits(&version.os, os) || !platform_admits(&version.cpu, cpu) {
            if optional {
                drop_edge(resolution, &edge);
                return Ok(());
            }
            warnings.push(InstallWarning::new(
                "PM_PLATFORM_MISMATCH",
                format!(
                    "`{}@{}` declares os {:?} cpu {:?}; this machine is {os} {cpu}",
                    package, version.version, version.os, version.cpu
                ),
            ));
        }
        let id = if aliased {
            PackageId::registry_alias(&edge.name, &package, &version.version)
        } else {
            PackageId::registry_version(&package, &version.version)
        };
        if !resolution.graph.packages.contains_key(&id) {
            add_registry_package(resolution, &self.project_root, &id, &edge.name, &version);
            if !aliased && let Ok(parsed) = semver::Version::parse(&version.version) {
                self.versions
                    .entry(package.clone())
                    .or_default()
                    .insert(parsed, id.clone());
            }
            for edge in registry_edges(&id, &version) {
                if edge.kind == PackageDependencyKind::Peer {
                    peers.push(edge);
                } else {
                    queue.push_back(edge);
                }
            }
        }
        link(resolution, &edge, &id);
        Ok(())
    }

    fn remove_unreached_placeholders(&self, resolution: &mut LocalResolution) {
        let reached: BTreeSet<&PackageId> = resolution
            .graph
            .dependencies
            .values()
            .flat_map(BTreeMap::values)
            .collect();
        let unreached: Vec<PackageId> = self
            .placeholders
            .iter()
            .filter(|id| !reached.contains(id))
            .cloned()
            .collect();
        for id in unreached {
            resolution.graph.packages.remove(&id);
            resolution.graph.dependencies.remove(&id);
            resolution.graph.dependency_kinds.remove(&id);
            resolution.lockfile.packages.remove(id.as_str());
        }
    }
}

/// The edges a resolved registry package declares: dependencies and optional
/// dependencies (an optional one replacing a dependency of the same name),
/// and the peers it does not mark optional.
fn registry_edges(from: &PackageId, version: &NpmPackageVersion) -> Vec<Edge> {
    let mut edges: BTreeMap<String, Edge> = BTreeMap::new();
    let mut add = |name: &String, spec: &String, kind| {
        edges.insert(
            name.clone(),
            Edge {
                from: from.clone(),
                name: name.clone(),
                spec: spec.clone(),
                kind,
                peer_pass: false,
            },
        );
    };
    for (name, spec) in &version.peer_dependencies {
        let optional = version
            .peer_dependencies_meta
            .get(name)
            .is_some_and(|meta| meta.optional);
        if !optional {
            add(name, spec, PackageDependencyKind::Peer);
        }
    }
    for (name, spec) in &version.dependencies {
        add(name, spec, PackageDependencyKind::Runtime);
    }
    for (name, spec) in &version.optional_dependencies {
        add(name, spec, PackageDependencyKind::Optional);
    }
    edges.into_values().collect()
}

fn link(resolution: &mut LocalResolution, edge: &Edge, target: &PackageId) {
    resolution.graph.insert_dependency_with_kind(
        edge.from.clone(),
        edge.name.clone(),
        target.clone(),
        edge.kind,
    );
    if let Some(package) = resolution.lockfile.packages.get_mut(edge.from.as_str()) {
        package
            .dependencies
            .insert(edge.name.clone(), target.to_string());
    }
}

fn drop_edge(resolution: &mut LocalResolution, edge: &Edge) {
    if let Some(dependencies) = resolution.graph.dependencies.get_mut(&edge.from) {
        dependencies.remove(&edge.name);
    }
    if let Some(kinds) = resolution.graph.dependency_kinds.get_mut(&edge.from) {
        kinds.remove(&edge.name);
    }
    if let Some(package) = resolution.lockfile.packages.get_mut(edge.from.as_str()) {
        package.dependencies.remove(&edge.name);
    }
}

fn add_registry_package(
    resolution: &mut LocalResolution,
    project_root: &Path,
    id: &PackageId,
    install_name: &str,
    version: &NpmPackageVersion,
) {
    let manifest = PackageManifest {
        name: Some(version.name.clone()),
        version: Some(version.version.clone()),
        dependencies: version.dependencies.clone(),
        peer_dependencies: version.peer_dependencies.clone(),
        optional_dependencies: version.optional_dependencies.clone(),
        bin: version.bin.clone(),
        scripts: version.scripts.clone(),
        ..PackageManifest::default()
    };
    let root = project_root
        .join("node_modules")
        .join(package_name_path(install_name));
    resolution.graph.insert_package(PackageRoot {
        id: id.clone(),
        name: install_name.to_string(),
        version: version.version.clone(),
        root: root.clone(),
        manifest: manifest.clone(),
    });
    insert_bins_for_manifest(&mut resolution.graph, id, &root, &manifest);
    resolution.lockfile.packages.insert(
        id.to_string(),
        LockedPackage {
            name: install_name.to_string(),
            version: version.version.clone(),
            dependencies: BTreeMap::new(),
            integrity: version.dist.integrity.clone().or_else(|| {
                version
                    .dist
                    .shasum
                    .as_ref()
                    .map(|sha| format!("sha1-{sha}"))
            }),
            attested: version.has_provenance(),
            resolved: version.dist.tarball.as_ref().map(|tarball| ResolvedSource {
                kind: ResolvedSourceKind::Registry,
                reference: tarball.clone(),
            }),
            lifecycle: LifecycleMetadata::from_scripts(&version.scripts, TrustState::Unknown),
        },
    );
}

/// The version of `metadata` that `range` resolves to: an exact version or a
/// dist-tag as named, otherwise `latest` when the range admits it, otherwise
/// the newest admitted release.
pub(crate) fn select_registry_version(
    metadata: &NpmRegistryMetadata,
    range: &str,
    gate: &ReleaseAgeGate,
    warnings: &mut Vec<InstallWarning>,
) -> Result<NpmPackageVersion, PackageManagerError> {
    if let Some(version) = metadata.versions.get(range) {
        return accept_sole_candidate(metadata, version, gate, warnings);
    }
    let tag = if matches!(range, "*" | "") {
        "latest"
    } else {
        range
    };
    if let Some(version) = metadata
        .dist_tags
        .get(tag)
        .and_then(|tagged| metadata.versions.get(tagged))
    {
        return accept_sole_candidate(metadata, version, gate, warnings);
    }
    if let Some(version) = select_semver_version(metadata, range, gate, warnings)? {
        return Ok(version);
    }
    Err(PackageManagerError::NoMatchingVersion {
        package: metadata.name.clone(),
        range: range.to_string(),
    })
}

/// Apply the age gate where the range leaves exactly one candidate.
///
/// There is nothing to fall back to, so a non-strict gate warns and proceeds
/// rather than failing an install whose version the project itself pinned.
fn accept_sole_candidate(
    metadata: &NpmRegistryMetadata,
    version: &NpmPackageVersion,
    gate: &ReleaseAgeGate,
    warnings: &mut Vec<InstallWarning>,
) -> Result<NpmPackageVersion, PackageManagerError> {
    if gate.is_disabled() || gate.admits(metadata, &version.version) {
        return Ok(version.clone());
    }
    let age_hours = gate.version_age_hours(metadata, &version.version);
    if gate.is_strict() {
        return Err(PackageManagerError::ReleaseTooNew {
            package: metadata.name.clone(),
            version: version.version.clone(),
            age_hours,
            minimum_hours: gate.window_hours(),
        });
    }
    warnings.push(InstallWarning::new(
        "PM_RELEASE_INSIDE_COOLING_WINDOW",
        format!(
            "`{}@{}` was published {age_hours}h ago and is the only version the range allows",
            metadata.name, version.version
        ),
    ));
    Ok(version.clone())
}

fn select_semver_version(
    metadata: &NpmRegistryMetadata,
    range: &str,
    gate: &ReleaseAgeGate,
    warnings: &mut Vec<InstallWarning>,
) -> Result<Option<NpmPackageVersion>, PackageManagerError> {
    let Some(req) = NpmRange::parse(range) else {
        return Ok(None);
    };
    // The version the publisher marked current wins whenever it is admitted.
    if let Some(latest) = metadata.dist_tags.get("latest")
        && let Ok(parsed) = semver::Version::parse(latest)
        && req.matches(&parsed)
        && (gate.is_disabled() || gate.admits(metadata, latest))
        && let Some(version) = metadata.versions.get(latest)
    {
        return Ok(Some(version.clone()));
    }
    let mut versions = metadata
        .versions
        .keys()
        .filter_map(|version| semver::Version::parse(version).ok())
        .filter(|version| req.matches(version))
        .collect::<Vec<_>>();
    versions.sort();
    let Some(newest) = versions.pop() else {
        return Ok(None);
    };
    let newest = newest.to_string();
    if gate.is_disabled() || gate.admits(metadata, &newest) {
        return Ok(metadata.versions.get(&newest).cloned());
    }

    let age_hours = gate.version_age_hours(metadata, &newest);
    if gate.is_strict() {
        return Err(PackageManagerError::ReleaseTooNew {
            package: metadata.name.clone(),
            version: newest,
            age_hours,
            minimum_hours: gate.window_hours(),
        });
    }
    // The newest match is still inside the cooling window, so fall back to the
    // newest match that has cleared it.
    while let Some(candidate) = versions.pop() {
        let candidate = candidate.to_string();
        if gate.admits(metadata, &candidate) {
            warnings.push(InstallWarning::new(
                "PM_RELEASE_HELD_BACK",
                format!(
                    "`{}` resolved to {candidate}: {newest} was published {age_hours}h ago, \
                     inside the {}h minimum release age",
                    metadata.name,
                    gate.window_hours()
                ),
            ));
            return Ok(metadata.versions.get(&candidate).cloned());
        }
    }
    Err(PackageManagerError::ReleaseTooNew {
        package: metadata.name.clone(),
        version: newest,
        age_hours,
        minimum_hours: gate.window_hours(),
    })
}
