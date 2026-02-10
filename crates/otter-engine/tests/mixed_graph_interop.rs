use std::path::Path;
use std::sync::Arc;

use otter_engine::{LoaderConfig, ModuleGraph, ModuleLoader};
use tempfile::tempdir;

fn file_url(path: &Path) -> String {
    format!("file://{}", path.canonicalize().unwrap().display())
}

#[tokio::test]
async fn test_mixed_graph_esm_to_cjs_interop_flags() {
    let dir = tempdir().unwrap();
    let entry = dir.path().join("entry.mjs");
    let lib = dir.path().join("lib.cjs");

    std::fs::write(&entry, "import mod from './lib.cjs'; export default mod;").unwrap();
    std::fs::write(&lib, "module.exports = { named: 1 };").unwrap();

    let loader = Arc::new(ModuleLoader::new(LoaderConfig {
        base_dir: dir.path().to_path_buf(),
        ..Default::default()
    }));
    let mut graph = ModuleGraph::new(loader);

    let entry_url = file_url(&entry);
    graph.load(&entry_url).await.unwrap();

    let entry_node = graph.get(&entry_url).unwrap();
    let rec = entry_node
        .import_records
        .iter()
        .find(|r| r.specifier == "./lib.cjs")
        .unwrap();

    assert!(!rec.is_require);
    assert!(rec.wrap_with_to_esm);
    assert!(!rec.wrap_with_to_commonjs);
}

#[tokio::test]
async fn test_mixed_graph_cjs_to_esm_interop_flags() {
    let dir = tempdir().unwrap();
    let entry = dir.path().join("entry.cjs");
    let lib = dir.path().join("lib.mjs");

    std::fs::write(
        &entry,
        "const mod = require('./lib.mjs'); module.exports = mod;",
    )
    .unwrap();
    std::fs::write(&lib, "export const named = 1; export default 2;").unwrap();

    let loader = Arc::new(ModuleLoader::new(LoaderConfig {
        base_dir: dir.path().to_path_buf(),
        ..Default::default()
    }));
    let mut graph = ModuleGraph::new(loader);

    let entry_url = file_url(&entry);
    graph.load(&entry_url).await.unwrap();

    let entry_node = graph.get(&entry_url).unwrap();
    let rec = entry_node
        .import_records
        .iter()
        .find(|r| r.specifier == "./lib.mjs")
        .unwrap();

    assert!(rec.is_require);
    assert!(!rec.wrap_with_to_esm);
    assert!(rec.wrap_with_to_commonjs);
}

#[tokio::test]
async fn test_mixed_graph_cycle_esm_cjs() {
    let dir = tempdir().unwrap();
    let a = dir.path().join("a.mjs");
    let b = dir.path().join("b.cjs");

    std::fs::write(&a, "import b from './b.cjs'; export default b;").unwrap();
    std::fs::write(
        &b,
        "const a = require('./a.mjs'); module.exports = { ok: !!a };",
    )
    .unwrap();

    let loader = Arc::new(ModuleLoader::new(LoaderConfig {
        base_dir: dir.path().to_path_buf(),
        ..Default::default()
    }));
    let mut graph = ModuleGraph::new(loader);

    let a_url = file_url(&a);
    let b_url = file_url(&b);
    graph.load(&a_url).await.unwrap();

    assert!(graph.contains(&a_url));
    assert!(graph.contains(&b_url));
    assert_eq!(graph.len(), 2);

    let order = graph.execution_order();
    assert_eq!(order.iter().filter(|u| **u == a_url).count(), 1);
    assert_eq!(order.iter().filter(|u| **u == b_url).count(), 1);
}

#[tokio::test]
async fn test_mixed_graph_cache_identity_single_shared_module() {
    let dir = tempdir().unwrap();
    let entry = dir.path().join("entry.mjs");
    let bridge = dir.path().join("bridge.cjs");
    let shared = dir.path().join("shared.cjs");

    std::fs::write(
        &entry,
        "import shared from './shared.cjs'; import './bridge.cjs'; export default shared;",
    )
    .unwrap();
    std::fs::write(
        &bridge,
        "const shared = require('./shared.cjs'); module.exports = shared;",
    )
    .unwrap();
    std::fs::write(&shared, "module.exports = { value: 1 };").unwrap();

    let loader = Arc::new(ModuleLoader::new(LoaderConfig {
        base_dir: dir.path().to_path_buf(),
        ..Default::default()
    }));
    let mut graph = ModuleGraph::new(loader);

    let entry_url = file_url(&entry);
    graph.load(&entry_url).await.unwrap();

    let shared_suffix = format!("{}", shared.file_name().unwrap().to_string_lossy());
    let shared_count = graph
        .modules()
        .filter(|(url, _)| url.ends_with(&shared_suffix))
        .count();

    assert_eq!(shared_count, 1);
}
