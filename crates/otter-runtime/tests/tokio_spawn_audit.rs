//! P2.2 Slice D regression coverage: every `tokio::spawn` site in
//! the active runtime/VM stack must be boundary-safe.
//!
//! ENGINE_REFACTOR_EXECUTION_PLAN §P2.2 acceptance:
//! "No `tokio::spawn` of work that touches `Interpreter` / `Value` /
//! `Local` outside the runtime scheduler boundary."
//!
//! Two layers protect that property:
//!
//! 1. The compile-fail test in
//!    `crates/otter-runtime/tests/compile_fail/tokio_spawn_native_ctx_is_not_send.rs`
//!    proves `NativeCtx` is `!Send`, so any future that captures it
//!    cannot satisfy `tokio::spawn`'s `Send + 'static` bound.
//! 2. *This* test enumerates every existing `tokio::spawn` /
//!    `tokio::task::spawn(_local|_blocking)?` / `Handle::spawn` site
//!    in the production source of active runtime/VM crates and
//!    pins the explicit allowlist so a new spawn site (which may
//!    or may not respect the Send boundary) cannot land silently.
//!
//! Adding a new spawn site requires updating this allowlist with a
//! short reason, which forces the contributor to think about the
//! VM/JS boundary the same way the plan demands.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Crates whose production source must be audited for spawn sites.
/// Test-only crates (`otter-test`, `otter-test262`) and the package
/// manager (`otter-pm*`) are intentionally excluded — they never
/// touch `Interpreter` / `Value` / `Local` directly.
const AUDITED_CRATES: &[&str] = &[
    "otter-bytecode",
    "otter-compiler",
    "otter-gc",
    "otter-macros",
    "otter-modules",
    "otter-runtime",
    "otter-syntax",
    "otter-vm",
    "otter-vm-codegen",
    "otter-web",
];

/// Spawn-shaped patterns the auditor matches. The compile-fail
/// test guarantees that any future capturing VM state cannot
/// satisfy the spawn signature; this list is what the *production*
/// code is allowed to call.
const SPAWN_PATTERNS: &[&str] = &[
    "tokio::spawn(",
    "tokio::task::spawn(",
    "tokio::task::spawn_local(",
    "tokio::task::spawn_blocking(",
    "self.handle.spawn(",
    "handle.spawn(",
];

/// Each entry: (relative path under workspace root) -> reason.
/// Adding a new entry is the explicit code-review checkpoint
/// described in the test docstring.
fn allowlist() -> BTreeMap<&'static str, &'static str> {
    let mut map = BTreeMap::new();
    map.insert(
        "crates/otter-runtime/src/event_loop.rs",
        "TokioEventLoop is the runtime's explicit Tokio boundary. Spawned work is limited \
         to timer sleeps and narrow host-service futures over owned data. Neither touches \
         Interpreter / Value / Local.",
    );
    map.insert(
        "crates/otter-modules/src/serve.rs",
        "Otter.serve's accept loop and per-connection tasks. Both spawns capture only \
         Send + 'static owned data: the std listener, a RuntimeTaskSpawner, an owned \
         RuntimeExecutionContext handle, Arc control/registry, and ServeRoots (opaque \
         RuntimePersistentRootId / symbol-root ids — never a Value/JsObject). No \
         Interpreter / Value / Local is captured or touched on the Tokio threads: every \
         VM re-entry is routed through RuntimeTaskSpawner::enqueue(ServeRequestTask), \
         whose RuntimeTask::run executes on the isolate thread under the scheduler \
         boundary. The hyper request is decoded into an owned HttpRequest (Strings + \
         bytes) before enqueue; JS Request/Response values are built only inside \
         ServeRequestTask::run.",
    );
    map
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("runtime crate must live under crates/")
        .to_path_buf()
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// `true` if `text[..idx]` is inside a `#[cfg(test)] mod tests {`
/// (or any inline test module). Heuristic: look for the nearest
/// `#[cfg(test)]` declaration before `idx`. If the next `mod ` or
/// closing `}` of equal indent has not yet been crossed before
/// `idx`, we are inside a cfg(test) block.
///
/// The contract is intentionally generous: false positives would
/// silently allow a spawn in production source. Production tests
/// in this workspace conform to the convention `#[cfg(test)] mod
/// tests {` at column 0; we match that exact prefix and assume
/// nested non-test modules above the spawn site keep the spawn
/// inside the test module.
fn is_inside_cfg_test_block(text: &str, idx: usize) -> bool {
    let head = &text[..idx];
    // Walk lines backwards and look for the most recent
    // `#[cfg(test)]` immediately followed by a `mod ` declaration.
    // If we hit a `}` at column 0 before finding `#[cfg(test)]`,
    // we are no longer inside that block.
    let mut nesting = 0i32;
    for line in head.lines().rev() {
        let trimmed = line.trim_end();
        if trimmed == "}" {
            nesting += 1;
            continue;
        }
        if trimmed.starts_with("#[cfg(test)]") && nesting == 0 {
            return true;
        }
        if trimmed.starts_with("mod ") && trimmed.ends_with("{") && nesting > 0 {
            nesting -= 1;
        }
    }
    false
}

#[derive(Debug)]
struct SpawnHit {
    relative_path: String,
    line: usize,
    pattern: &'static str,
    snippet: String,
}

#[test]
fn audit_tokio_spawn_sites_in_active_crates() {
    let root = workspace_root();
    let allowed = allowlist();
    let mut hits: Vec<SpawnHit> = Vec::new();

    for crate_name in AUDITED_CRATES {
        let crate_src = root.join("crates").join(crate_name).join("src");
        if !crate_src.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        collect_rs_files(&crate_src, &mut files);
        for file in files {
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => text,
                Err(_) => continue,
            };
            for pattern in SPAWN_PATTERNS {
                let mut search_from = 0;
                while let Some(rel_idx) = text[search_from..].find(pattern) {
                    let abs_idx = search_from + rel_idx;
                    search_from = abs_idx + pattern.len();
                    let line_no = text[..abs_idx].matches('\n').count() + 1;
                    let line_start = text[..abs_idx].rfind('\n').map_or(0, |n| n + 1);
                    let line_end = text[abs_idx..]
                        .find('\n')
                        .map_or(text.len(), |n| abs_idx + n);
                    let snippet = text[line_start..line_end].trim().to_string();
                    // Skip references inside doc-comments / inline
                    // comments — they explain the policy, they do
                    // not invoke spawn.
                    if snippet.starts_with("//") || snippet.starts_with("///") {
                        continue;
                    }
                    if is_inside_cfg_test_block(&text, abs_idx) {
                        continue;
                    }
                    let relative_path = file
                        .strip_prefix(&root)
                        .unwrap_or(&file)
                        .to_string_lossy()
                        .replace('\\', "/");
                    hits.push(SpawnHit {
                        relative_path,
                        line: line_no,
                        pattern,
                        snippet,
                    });
                }
            }
        }
    }

    let mut violations: Vec<String> = Vec::new();
    for hit in &hits {
        if !allowed.contains_key(hit.relative_path.as_str()) {
            violations.push(format!(
                "{}:{}: forbidden spawn `{}` in production source — `{}`. \
                 Add an allowlist entry in tokio_spawn_audit.rs with a Send/safety \
                 justification, or route the work through the runtime scheduler.",
                hit.relative_path, hit.line, hit.pattern, hit.snippet
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "tokio spawn audit failed:\n  {}",
        violations.join("\n  ")
    );

    // Belt-and-braces: every allowlisted file must actually
    // contain at least one spawn site. If a site is removed, the
    // allowlist row becomes stale and should be deleted.
    let observed_files: std::collections::BTreeSet<&str> =
        hits.iter().map(|h| h.relative_path.as_str()).collect();
    for entry in allowed.keys() {
        assert!(
            observed_files.contains(entry),
            "allowlist entry {entry} no longer matches any spawn site; remove it from \
             tokio_spawn_audit.rs"
        );
    }
}
