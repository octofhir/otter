//! `package.json` + `pnpm-workspace.yaml` types for Otter's package manager.
//!
//! The one-crate-owns-one-responsibility split starts here: every other
//! PM crate pulls its manifest types from this crate, so a schema change
//! touches one file.

pub mod workspace;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub use workspace::WorkspaceConfig;

/// Parsed `package.json`.
///
/// Everything beyond `name`/`version` is optional — we only deserialize
/// fields the PM needs. Unknown keys pass through untouched (serde
/// ignores them by default) so we never reject a valid manifest.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PackageJson {
    pub name: Option<String>,
    pub version: Option<String>,

    /// The directory the manifest was loaded from. Not part of the JSON
    /// shape; set by [`PackageJson::from_path`] so downstream code can
    /// resolve `file:` / `link:` specs without threading the path through
    /// every call site. Flagged `skip` so round-trips don't emit it.
    #[serde(skip)]
    pub dir: PathBuf,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,

    #[serde(
        rename = "devDependencies",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub dev_dependencies: BTreeMap<String, String>,

    #[serde(
        rename = "optionalDependencies",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub optional_dependencies: BTreeMap<String, String>,

    #[serde(
        rename = "peerDependencies",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub peer_dependencies: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts: Option<HashMap<String, String>>,

    /// Binary entry points — string (single) or object (multiple).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<BinField>,

    pub main: Option<String>,

    /// ESM vs CJS (`"commonjs"` / `"module"`).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub pkg_type: Option<String>,

    /// `workspaces` field — present in npm / yarn / bun projects.
    /// pnpm puts the equivalent in `pnpm-workspace.yaml` instead and
    /// this stays `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Workspaces>,

    /// npm-style top-level `overrides` block (also used by yarn's
    /// `resolutions` — we merge both at read time). Values can be a
    /// version spec or a nested object for path-qualified overrides;
    /// we keep the raw JSON and let the resolver interpret it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<serde_json::Value>,

    /// Yarn's equivalent of `overrides`. Kept separate on ingest so
    /// [`PackageJson::overrides_map`] can merge both into a canonical
    /// `BTreeMap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolutions: Option<serde_json::Value>,

    /// pnpm-specific configuration (`pnpm.overrides`, `pnpm.patchedDependencies`,
    /// `pnpm.ignoredOptionalDependencies`, etc.). Kept as raw JSON and
    /// walked lazily because this block is open-ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pnpm: Option<serde_json::Value>,

    /// `bundledDependencies` / legacy `bundleDependencies`. Both spellings
    /// are accepted by npm and appear in the wild; we normalize on read.
    #[serde(
        rename = "bundledDependencies",
        alias = "bundleDependencies",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub bundled_dependencies: Option<BundledDependencies>,

    /// Preserve extra JSON fields (e.g. `"files"`, `"exports"`, custom keys)
    /// so round-trip writes don't silently drop them. Every registered
    /// field above is `skip`-ed from this bag on deserialize.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl PackageJson {
    /// Load and parse `<dir>/package.json`. The loaded manifest has
    /// `dir` populated with the parent directory of `path`.
    pub fn from_path(path: &Path) -> Result<Self, Error> {
        let bytes =
            std::fs::read(path).map_err(|e| Error::Io(path.to_path_buf(), e.to_string()))?;
        let mut pkg: PackageJson = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;
        pkg.dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(pkg)
    }

    /// Iterate direct deps across prod / dev / optional / peer. Each
    /// tuple is `(name, raw_spec)`. Duplicates are emitted for deps
    /// listed in more than one block — downstream code decides which
    /// wins (spec is: prod > peer > optional > dev).
    pub fn all_dependencies(&self) -> impl Iterator<Item = (&str, &str)> {
        self.dependencies
            .iter()
            .chain(self.dev_dependencies.iter())
            .chain(self.optional_dependencies.iter())
            .chain(self.peer_dependencies.iter())
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Merge `overrides` + `resolutions` + `pnpm.overrides` into a flat
    /// `selector → spec` map. Nested-object values from `overrides` are
    /// flattened to `parent>child` keys to match pnpm's selector syntax.
    /// Later sources win on conflict: `pnpm.overrides` > `overrides` > `resolutions`.
    pub fn overrides_map(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(v) = &self.resolutions {
            flatten_overrides(v, String::new(), &mut out);
        }
        if let Some(v) = &self.overrides {
            flatten_overrides(v, String::new(), &mut out);
        }
        if let Some(pnpm) = self.pnpm.as_ref()
            && let Some(v) = pnpm.get("overrides")
        {
            flatten_overrides(v, String::new(), &mut out);
        }
        out
    }

    /// Names listed in `pnpm.ignoredOptionalDependencies`, as a set for
    /// quick containment checks during resolve. Empty when unset.
    pub fn pnpm_ignored_optional_dependencies(&self) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        if let Some(pnpm) = self.pnpm.as_ref()
            && let Some(arr) = pnpm
                .get("ignoredOptionalDependencies")
                .and_then(|v| v.as_array())
        {
            for item in arr {
                if let Some(s) = item.as_str() {
                    out.insert(s.to_string());
                }
            }
        }
        out
    }
}

/// Flatten a JSON overrides/resolutions block into `prefix[>name]` → spec.
fn flatten_overrides(
    value: &serde_json::Value,
    prefix: String,
    out: &mut BTreeMap<String, String>,
) {
    let Some(obj) = value.as_object() else {
        return;
    };
    for (k, v) in obj {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}>{k}")
        };
        match v {
            serde_json::Value::String(s) => {
                out.insert(key, s.clone());
            }
            serde_json::Value::Object(_) => {
                flatten_overrides(v, key, out);
            }
            _ => {}
        }
    }
}

/// `package.json#workspaces` field. npm and bun accept a bare string
/// array; yarn additionally accepts a `{ packages: [...] }` object.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Workspaces {
    /// `"workspaces": ["packages/*"]`.
    Array(Vec<String>),
    /// `"workspaces": { "packages": ["packages/*"], "nohoist": [...] }`.
    Object {
        #[serde(default)]
        packages: Vec<String>,
        /// yarn classic's opt-out from hoisting; captured for parity but
        /// not yet honored by Otter's linker.
        #[serde(default)]
        nohoist: Vec<String>,
    },
}

impl Workspaces {
    pub fn patterns(&self) -> &[String] {
        match self {
            Workspaces::Array(v) => v,
            Workspaces::Object { packages, .. } => packages,
        }
    }
}

/// `bin` entry — either a single string or a name → path map.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum BinField {
    Single(String),
    Multiple(HashMap<String, String>),
}

impl BinField {
    /// Resolve to `command → path`. For `Single`, the command name is
    /// the package's basename (scope stripped) per the npm spec.
    pub fn to_map(&self, package_name: &str) -> HashMap<String, String> {
        match self {
            BinField::Single(path) => {
                let cmd = package_name.split('/').next_back().unwrap_or(package_name);
                HashMap::from([(cmd.to_string(), path.clone())])
            }
            BinField::Multiple(m) => m.clone(),
        }
    }
}

/// `bundledDependencies` — either a name list, or `true` (meaning
/// "bundle everything in `dependencies`"). The latter is rare but
/// present in npm's schema; we preserve the original shape for
/// round-trips and let [`BundledDependencies::names`] resolve it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum BundledDependencies {
    /// Bundle all declared `dependencies`.
    All(bool),
    /// Bundle exactly these names.
    List(Vec<String>),
}

impl BundledDependencies {
    /// Resolve to a concrete name list, consulting `dependencies` when
    /// the field is `true`.
    pub fn names<'a>(&'a self, dependencies: &'a BTreeMap<String, String>) -> Vec<&'a str> {
        match self {
            BundledDependencies::All(true) => dependencies.keys().map(String::as_str).collect(),
            BundledDependencies::All(false) => Vec::new(),
            BundledDependencies::List(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error at {0}: {1}")]
    Io(PathBuf, String),
    #[error("failed to parse {0}: {1}")]
    Parse(PathBuf, String),
    #[error("failed to parse YAML at {0}: {1}")]
    YamlParse(PathBuf, String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_package_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(&path, r#"{"name":"foo","version":"1.0.0"}"#).unwrap();
        let pkg = PackageJson::from_path(&path).unwrap();
        assert_eq!(pkg.name.as_deref(), Some("foo"));
        assert_eq!(pkg.dir, dir.path());
    }

    #[test]
    fn parses_workspaces_array_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"root","workspaces":["packages/*","apps/*"]}"#,
        )
        .unwrap();
        let pkg = PackageJson::from_path(&path).unwrap();
        assert_eq!(
            pkg.workspaces.as_ref().map(|w| w.patterns()),
            Some(&["packages/*".to_string(), "apps/*".to_string()][..])
        );
    }

    #[test]
    fn parses_workspaces_object_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"root","workspaces":{"packages":["packages/*"],"nohoist":["**/foo"]}}"#,
        )
        .unwrap();
        let pkg = PackageJson::from_path(&path).unwrap();
        assert_eq!(
            pkg.workspaces.as_ref().map(|w| w.patterns()),
            Some(&["packages/*".to_string()][..])
        );
    }

    #[test]
    fn all_dependencies_walks_every_block() {
        let json = r#"{
            "name":"x","version":"0.0.0",
            "dependencies":{"a":"1"},
            "devDependencies":{"b":"2"},
            "optionalDependencies":{"c":"3"},
            "peerDependencies":{"d":"4"}
        }"#;
        let pkg: PackageJson = serde_json::from_str(json).unwrap();
        let names: Vec<&str> = pkg.all_dependencies().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn overrides_map_merges_all_sources() {
        let json = r#"{
            "name":"x","version":"0.0.0",
            "resolutions":{"foo":"1.0.0"},
            "overrides":{"bar":"2.0.0","baz":{"qux":"3.0.0"}},
            "pnpm":{"overrides":{"foo":"9.9.9"}}
        }"#;
        let pkg: PackageJson = serde_json::from_str(json).unwrap();
        let map = pkg.overrides_map();
        assert_eq!(map.get("foo").map(String::as_str), Some("9.9.9"));
        assert_eq!(map.get("bar").map(String::as_str), Some("2.0.0"));
        assert_eq!(map.get("baz>qux").map(String::as_str), Some("3.0.0"));
    }

    #[test]
    fn bin_single_uses_unscoped_name() {
        let b = BinField::Single("./cli.js".to_string());
        let m = b.to_map("@scope/pkg");
        assert_eq!(m.get("pkg").map(String::as_str), Some("./cli.js"));
    }

    #[test]
    fn pnpm_ignored_optional_parses() {
        let json = r#"{
            "name":"x","version":"0.0.0",
            "pnpm":{"ignoredOptionalDependencies":["fsevents","esbuild"]}
        }"#;
        let pkg: PackageJson = serde_json::from_str(json).unwrap();
        let ignored = pkg.pnpm_ignored_optional_dependencies();
        assert!(ignored.contains("fsevents"));
        assert!(ignored.contains("esbuild"));
    }
}
