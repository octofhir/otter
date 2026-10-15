//! Browser-hosted ES module graphs: the embedder delivers every source, the
//! engine never fetches, and nothing blocks while a graph is incomplete.

use otter_runtime::web_modules::{WebModuleError, WebModuleGraph, WebModuleMap, WebModuleSource};
use otter_runtime::{OtterError, Runtime, SourceInput};

const PAGE: &str = "https://site.test/app/index.html";

fn page() -> (Runtime, WebModuleMap) {
    let map = WebModuleMap::new();
    map.set_document_base(PAGE);
    let mut runtime = Runtime::builder()
        .web_modules(map.clone())
        .build()
        .expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript("globalThis.log = [];"),
            "https://site.test/app/setup.js",
        )
        .expect("setup");
    (runtime, map)
}

fn deliver(map: &WebModuleMap, url: &str, text: &str) {
    deliver_from(map, url, url, text);
}

fn deliver_from(map: &WebModuleMap, request: &str, final_url: &str, text: &str) {
    map.deliver(
        request,
        Ok(WebModuleSource {
            final_url: final_url.to_owned(),
            content_type: Some("text/javascript".to_owned()),
            text: text.to_owned(),
        }),
    );
}

fn eval(runtime: &mut Runtime, source: &str) -> String {
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "https://site.test/eval.js",
        )
        .expect("eval")
        .completion_string()
        .to_owned()
}

fn log(runtime: &mut Runtime) -> String {
    eval(runtime, "log.join(',')")
}

/// Run the prepare/deliver loop for an entry the way an embedder does,
/// answering each requested URL from `files`.
fn load_graph(map: &WebModuleMap, entry: &str, files: &[(&str, &str)]) -> Vec<String> {
    let mut asked = Vec::new();
    loop {
        match map.graph_status(entry).expect("graph") {
            WebModuleGraph::Ready => return asked,
            WebModuleGraph::Pending { fetch } => {
                assert!(!fetch.is_empty(), "a pending graph with nothing to fetch");
                for url in fetch {
                    let text = files
                        .iter()
                        .find(|(name, _)| *name == url)
                        .unwrap_or_else(|| panic!("unexpected fetch {url}"))
                        .1;
                    deliver(map, &url, text);
                    asked.push(url);
                }
            }
        }
    }
}

fn run_inline(runtime: &mut Runtime, map: &WebModuleMap, ordinal: u64, text: &str) {
    let url = WebModuleMap::inline_url(PAGE, ordinal);
    deliver(map, &url, text);
    runtime
        .run_module_source(SourceInput::from_javascript(text), url)
        .expect("module runs");
}

#[test]
fn an_inline_module_with_relative_and_absolute_imports_runs_from_delivered_sources() {
    let (mut runtime, map) = page();
    let entry = WebModuleMap::inline_url(PAGE, 0);
    let text = r#"
        import { a } from "./lib/a.js";
        import { b } from "/root/b.js";
        import { c } from "https://cdn.test/c.js";
        log.push(a, b, c);
    "#;
    deliver(&map, &entry, text);
    let asked = load_graph(
        &map,
        &entry,
        &[
            ("https://site.test/app/lib/a.js", "export const a = 'a';"),
            ("https://site.test/root/b.js", "export const b = 'b';"),
            ("https://cdn.test/c.js", "export const c = 'c';"),
        ],
    );
    assert_eq!(asked.len(), 3);
    runtime
        .run_module_source(SourceInput::from_javascript(text), entry)
        .expect("module runs");
    assert_eq!(log(&mut runtime), "a,b,c");
}

#[test]
fn a_dependency_is_discovered_only_after_its_importer_is_delivered() {
    let map = WebModuleMap::new();
    map.set_document_base(PAGE);
    let entry = "https://site.test/app/main.js";
    assert_eq!(
        map.graph_status(entry).unwrap(),
        WebModuleGraph::Pending {
            fetch: vec![entry.to_owned()]
        }
    );
    // Asked once: a second look reports nothing new while it is in flight.
    assert_eq!(
        map.graph_status(entry).unwrap(),
        WebModuleGraph::Pending { fetch: vec![] }
    );
    deliver(&map, entry, "import './dep.js';");
    assert_eq!(
        map.graph_status(entry).unwrap(),
        WebModuleGraph::Pending {
            fetch: vec!["https://site.test/app/dep.js".to_owned()]
        }
    );
    deliver(&map, "https://site.test/app/dep.js", "");
    assert_eq!(map.graph_status(entry).unwrap(), WebModuleGraph::Ready);
}

#[test]
fn a_redirected_module_resolves_its_imports_from_where_it_landed() {
    let (mut runtime, map) = page();
    let entry = WebModuleMap::inline_url(PAGE, 0);
    let text = "import { where, dep } from './old.js'; log.push(where, dep);";
    deliver(&map, &entry, text);
    deliver_from(
        &map,
        "https://site.test/app/old.js",
        "https://site.test/v2/new.js",
        "export { dep } from './dep.js'; export const where = import.meta.url;",
    );
    let asked = load_graph(
        &map,
        &entry,
        &[("https://site.test/v2/dep.js", "export const dep = 'v2';")],
    );
    assert_eq!(asked, ["https://site.test/v2/dep.js"]);
    runtime
        .run_module_source(SourceInput::from_javascript(text), entry)
        .expect("module runs");
    assert_eq!(log(&mut runtime), "https://site.test/v2/new.js,v2");
}

#[test]
fn a_cycle_evaluates_each_module_once_in_post_order() {
    let (mut runtime, map) = page();
    let entry = WebModuleMap::inline_url(PAGE, 0);
    let text = "import './a.js'; log.push('entry');";
    deliver(&map, &entry, text);
    load_graph(
        &map,
        &entry,
        &[
            (
                "https://site.test/app/a.js",
                "import { b } from './b.js'; export const a = 'a'; log.push('a:' + b());",
            ),
            (
                "https://site.test/app/b.js",
                "import { a } from './a.js'; export function b() { return 'b+' + a; } log.push('b');",
            ),
        ],
    );
    runtime
        .run_module_source(SourceInput::from_javascript(text), entry)
        .expect("module runs");
    assert_eq!(log(&mut runtime), "b,a:b+a,entry");
}

#[test]
fn modules_shared_between_scripts_are_fetched_and_evaluated_once() {
    let (mut runtime, map) = page();
    let shared = "https://site.test/app/shared.js";
    deliver(
        &map,
        shared,
        "globalThis.count = (globalThis.count ?? 0) + 1; export const n = count;",
    );
    let first = "import { n } from './shared.js'; log.push('one:' + n);";
    let second = "import { n } from './shared.js'; log.push('two:' + n);";
    for (ordinal, text) in [(0, first), (1, second)] {
        let url = WebModuleMap::inline_url(PAGE, ordinal);
        deliver(&map, &url, text);
        assert_eq!(map.graph_status(&url).unwrap(), WebModuleGraph::Ready);
        runtime
            .run_module_source(SourceInput::from_javascript(text), url)
            .expect("module runs");
    }
    assert_eq!(log(&mut runtime), "one:1,two:1");
    assert_eq!(eval(&mut runtime, "count"), "1");
}

#[test]
fn two_inline_modules_are_separate_records_with_the_document_url() {
    let (mut runtime, map) = page();
    run_inline(&mut runtime, &map, 0, "log.push(import.meta.url);");
    run_inline(&mut runtime, &map, 1, "log.push('second');");
    assert_eq!(log(&mut runtime), format!("{PAGE},second"));
}

#[test]
fn an_import_map_rewrites_bare_and_prefixed_specifiers() {
    let (mut runtime, map) = page();
    map.register_import_map(
        r#"{"imports": {"lib": "/vendor/lib.js", "pkg/": "https://cdn.test/pkg/"},
            "scopes": {"https://cdn.test/": {"lib": "https://cdn.test/lib-for-cdn.js"}}}"#,
        PAGE,
    )
    .expect("import map");
    let entry = WebModuleMap::inline_url(PAGE, 0);
    let text = "import lib from 'lib'; import x from 'pkg/x.js'; log.push(lib, x);";
    deliver(&map, &entry, text);
    load_graph(
        &map,
        &entry,
        &[
            (
                "https://site.test/vendor/lib.js",
                "export default 'page-lib';",
            ),
            (
                "https://cdn.test/pkg/x.js",
                "import lib from 'lib'; export default 'x+' + lib;",
            ),
            (
                "https://cdn.test/lib-for-cdn.js",
                "export default 'cdn-lib';",
            ),
        ],
    );
    runtime
        .run_module_source(SourceInput::from_javascript(text), entry)
        .expect("module runs");
    assert_eq!(log(&mut runtime), "page-lib,x+cdn-lib");
}

#[test]
fn an_unmapped_bare_specifier_fails_the_graph_as_a_resolution_error() {
    let map = WebModuleMap::new();
    let entry = WebModuleMap::inline_url(PAGE, 0);
    deliver(&map, &entry, "import 'react';");
    let error = map.graph_status(&entry).unwrap_err();
    assert!(
        matches!(&error, WebModuleError::Resolve { message, .. } if message.contains("react")),
        "{error:?}"
    );
}

#[test]
fn a_failed_fetch_and_a_syntax_error_fail_the_graph_with_their_kind() {
    let map = WebModuleMap::new();
    let entry = WebModuleMap::inline_url(PAGE, 0);
    deliver(&map, &entry, "import './missing.js'; import './broken.js';");
    let _ = map.graph_status(&entry);
    map.deliver(
        "https://site.test/app/missing.js",
        Err("HTTP 404".to_owned()),
    );
    deliver(&map, "https://site.test/app/broken.js", "export const = ;");
    let error = map.graph_status(&entry).unwrap_err();
    assert!(
        matches!(&error, WebModuleError::Fetch { url, message }
            if url.ends_with("missing.js") && message == "HTTP 404")
            || matches!(&error, WebModuleError::Parse { url, .. } if url.ends_with("broken.js")),
        "{error:?}"
    );
    let only_broken = WebModuleMap::new();
    let entry = WebModuleMap::inline_url(PAGE, 1);
    deliver(&only_broken, &entry, "export const = ;");
    assert!(matches!(
        only_broken.graph_status(&entry),
        Err(WebModuleError::Parse { .. })
    ));
}

#[test]
fn a_module_is_strict_and_its_top_level_is_not_global() {
    let (mut runtime, map) = page();
    run_inline(
        &mut runtime,
        &map,
        0,
        r#"
        var hidden = 1;
        log.push(typeof this, typeof globalThis.hidden);
        try { undeclared = 1; log.push('sloppy'); } catch (e) { log.push(e.constructor.name); }
        "#,
    );
    assert_eq!(log(&mut runtime), "undefined,undefined,ReferenceError");
}

#[test]
fn an_evaluation_error_is_a_runtime_error_and_later_modules_still_run() {
    let (mut runtime, map) = page();
    let url = WebModuleMap::inline_url(PAGE, 0);
    let text = "log.push('before'); throw new TypeError('boom');";
    deliver(&map, &url, text);
    let error = runtime
        .run_module_source(SourceInput::from_javascript(text), url)
        .unwrap_err();
    assert!(matches!(error, OtterError::Runtime { .. }), "{error:?}");
    run_inline(&mut runtime, &map, 1, "log.push('after');");
    assert_eq!(log(&mut runtime), "before,after");
}

#[test]
fn top_level_await_suspends_the_module_until_its_promise_settles() {
    let (mut runtime, map) = page();
    run_inline(
        &mut runtime,
        &map,
        0,
        r#"
        log.push('start');
        await new Promise((resolve) => { globalThis.release = resolve; });
        log.push('resumed');
        "#,
    );
    assert_eq!(log(&mut runtime), "start");
    eval(&mut runtime, "release(); undefined");
    runtime.run_microtasks().expect("microtasks");
    assert_eq!(log(&mut runtime), "start,resumed");
}

#[test]
fn a_dynamic_import_parks_until_its_graph_is_delivered() {
    let (mut runtime, map) = page();
    run_inline(
        &mut runtime,
        &map,
        0,
        r#"
        import("./lazy.js").then(
            (m) => log.push('loaded:' + m.value),
            (e) => log.push('failed:' + e));
        log.push('requested');
        "#,
    );
    assert_eq!(log(&mut runtime), "requested");
    assert!(runtime.has_pending_web_imports());
    let fetch = runtime.web_module_fetches();
    assert_eq!(fetch, ["https://site.test/app/lazy.js"]);
    assert!(runtime.web_module_fetches().is_empty(), "reported once");
    deliver(
        &map,
        "https://site.test/app/lazy.js",
        "export { value } from './dep.js';",
    );
    runtime.resume_web_dynamic_imports().expect("resume");
    assert_eq!(log(&mut runtime), "requested");
    assert_eq!(
        runtime.web_module_fetches(),
        ["https://site.test/app/dep.js"]
    );
    deliver(
        &map,
        "https://site.test/app/dep.js",
        "export const value = 42;",
    );
    runtime.resume_web_dynamic_imports().expect("resume");
    assert!(!runtime.has_pending_web_imports());
    assert_eq!(log(&mut runtime), "requested,loaded:42");
}

#[test]
fn a_dynamic_import_from_a_classic_script_resolves_against_the_document() {
    let (mut runtime, map) = page();
    map.register_import_map(r#"{"imports": {"lazy": "./lazy.js"}}"#, PAGE)
        .expect("import map");
    eval(
        &mut runtime,
        "import('lazy').then((m) => log.push(m.default)); undefined",
    );
    assert_eq!(
        runtime.web_module_fetches(),
        ["https://site.test/app/lazy.js"]
    );
    deliver(
        &map,
        "https://site.test/app/lazy.js",
        "export default 'lazy';",
    );
    runtime.resume_web_dynamic_imports().expect("resume");
    assert_eq!(log(&mut runtime), "lazy");
}

#[test]
fn an_already_evaluated_module_is_imported_without_a_fetch() {
    let (mut runtime, map) = page();
    deliver(
        &map,
        "https://site.test/app/once.js",
        "globalThis.evaluations = (globalThis.evaluations ?? 0) + 1; export const v = 'once';",
    );
    run_inline(&mut runtime, &map, 0, "import './once.js';");
    run_inline(
        &mut runtime,
        &map,
        1,
        "import('./once.js').then((m) => log.push(m.v, evaluations));",
    );
    assert!(!runtime.has_pending_web_imports());
    assert_eq!(log(&mut runtime), "once,1");
}

#[test]
fn a_failed_dynamic_import_rejects_with_a_type_error() {
    let (mut runtime, map) = page();
    run_inline(
        &mut runtime,
        &map,
        0,
        "import('./gone.js').catch((e) => log.push(e.constructor.name));",
    );
    assert_eq!(
        runtime.web_module_fetches(),
        ["https://site.test/app/gone.js"]
    );
    map.deliver("https://site.test/app/gone.js", Err("HTTP 404".to_owned()));
    runtime.resume_web_dynamic_imports().expect("resume");
    assert_eq!(log(&mut runtime), "TypeError");
}

#[test]
fn a_dynamic_import_queued_by_a_microtask_settles_in_the_same_turn() {
    let (mut runtime, map) = page();
    eval(
        &mut runtime,
        "globalThis.later = () => import('./later.js').then((m) => log.push(m.default)); undefined",
    );
    // `eval.js` sits at the site root, so that is where `./later.js` is.
    deliver(
        &map,
        "https://site.test/later.js",
        "export default 'later';",
    );
    eval(&mut runtime, "Promise.resolve().then(later); undefined");
    assert_eq!(log(&mut runtime), "later");
}

#[test]
fn a_parameter_shadows_an_import_binding_of_the_same_name() {
    let (mut runtime, map) = page();
    deliver(&map, "https://site.test/app/ns.js", "export const x = 1;");
    run_inline(
        &mut runtime,
        &map,
        0,
        r#"
        import * as s from './ns.js';
        import { x as t } from './ns.js';
        const methods = { m(e, t, s) { return typeof s + ':' + typeof t; } };
        function plain(s) { return typeof s; }
        const arrow = (s) => typeof s;
        log.push(methods.m(1, 'two', () => 0), plain(3), arrow('x'), typeof s, t);
        "#,
    );
    assert_eq!(log(&mut runtime), "function:string,number,string,object,1");
}
