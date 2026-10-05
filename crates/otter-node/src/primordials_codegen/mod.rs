//! Owned offline AST coverage and source emission for Node primordials.
//!
//! # Contents
//! - Exact current source reads and deterministic coverage records.
//! - One parser callback boundary, with no AST arena escaping.
//! - Static lookup recipes emitted into the current compat bootstrap.
//!
//! # Invariants
//! The producer never executes JavaScript. Only the existing lazy Map/Proxy
//! owns runtime values. Unknown names return undefined; there is no generic
//! name decoder, runtime recipe registry or fallback format.
//!
//! # See also
//! - `collect` validates ambient bindings and the sole dynamic read proof.
//! - `resolve` preserves lookup-stage and holder-prefix priority.
//! - `emit` replaces current AST spans idempotently.

mod collect;
mod emit;
mod resolve;
#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, path::Path};

use otter_syntax::{SourceGoal, SourceKind, with_program_goal};
use oxc_ast::ast::Program;
use oxc_span::Span;
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Clone)]
struct Site {
    path: String,
    start: u32,
    end: u32,
    kind: &'static str,
}

#[derive(Default)]
struct Coverage {
    names: BTreeMap<String, Vec<Site>>,
    sources: BTreeMap<String, String>,
    dynamic: Vec<Site>,
}

impl Coverage {
    fn note(&mut self, name: &str, site: Site) {
        self.names.entry(name.to_owned()).or_default().push(site);
    }
}

/// One emitted source and its exact tooling-only input manifest.
pub(super) struct Generated {
    pub(super) source: String,
    manifest: serde_json::Value,
}

impl Generated {
    pub(super) fn manifest_text(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(&self.manifest)? + "\n")
    }
}

fn digest(source: &str) -> String {
    format!("{:x}", Sha256::digest(source.as_bytes()))
}

fn parse<T>(source: &str, consume: impl for<'a> FnOnce(&Program<'a>) -> Result<T>) -> Result<T> {
    with_program_goal(
        source,
        SourceKind::JavaScript,
        SourceGoal::Script,
        |program| {
            if program.source_text != source {
                return Err("AST span input was changed by a parser compatibility rewrite".into());
            }
            consume(program)
        },
    )
    .map_err(|error| format!("Script parse: {error:?}"))?
}

fn reject(path: &str, span: Span, reason: &str) -> Box<dyn std::error::Error> {
    format!("{path}:{}..{}: {reason}", span.start, span.end).into()
}

/// Generate from current bootstrap bytes plus the complete in-repo Node source
/// census. Output is independent of directory traversal and hash-map order.
pub(super) fn generate(repo: &Path, bootstrap: &Path) -> Result<Generated> {
    let source = std::fs::read_to_string(bootstrap)?;
    let facts = parse(&source, resolve::Facts::read)?;
    let base = repo.join("crates/otter-node/src/nodelib");
    let (owner, mut paths) = collect::source_paths(repo)?;
    paths.sort();
    let mut coverage = Coverage::default();
    coverage
        .sources
        .insert("../nodelib.rs".to_owned(), digest(&owner));
    for path in paths {
        let relative = path
            .strip_prefix(&base)?
            .to_string_lossy()
            .replace('\\', "/");
        if relative == "compat/bootstrap_realm.js" {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        coverage.sources.insert(relative.clone(), digest(&text));
        collect::inspect(&relative, &text, &facts.typed_arrays(), &mut coverage)?;
    }
    for name in facts.special_names() {
        coverage.names.entry(name).or_default();
    }
    let cases: Vec<_> = coverage.names.keys().map(|name| facts.case(name)).collect();
    let output = emit::rewrite(&source, &cases)?;
    let mut uses = Vec::new();
    for (name, sites) in &coverage.names {
        uses.push(serde_json::json!({
            "name": name,
            "uses": sites.iter().map(|site| serde_json::json!({
                "source":site.path,"start":site.start,"end":site.end,"kind":site.kind
            })).collect::<Vec<_>>(),
        }));
    }
    let manifest = serde_json::json!({
        "bootstrapInputSha256":digest(&source),
        "bootstrapOutputSha256":digest(&output),
        "sourceSha256":coverage.sources,
        "catalog":uses,
        "dynamicReads":coverage.dynamic.iter().map(|site| serde_json::json!({
            "source":site.path,"start":site.start,"end":site.end,"kind":site.kind
        })).collect::<Vec<_>>(),
        "recipes":cases.iter().map(resolve::Case::record).collect::<Vec<_>>(),
        "contract":"closed in-repo primordial catalog; unknown names are undefined",
    });
    Ok(Generated {
        source: output,
        manifest,
    })
}
