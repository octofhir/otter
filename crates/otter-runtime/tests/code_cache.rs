//! A script an embedder ran once runs again from the bytes it kept,
//! without compiling.
//!
//! # Invariants
//!
//! - The first run of a script hands back bytes; a run given them compiles
//!   nothing and behaves the same, lazily compiled functions included.
//! - Bytes taken after the script's functions ran carry their bodies: a run
//!   given those compiles none of them either. Two different scripts run
//!   under one address keep no bodies for it.
//! - A large indirect `eval` is kept through the code-cache hook, and its
//!   functions' bodies with it once they ran.
//! - A script prepared on another thread runs as the same script run from
//!   `run_script_cached` would.
//! - The same source under another specifier runs from the same bytes, and
//!   its functions name the specifier it ran under.
//! - Bytes for another source or a damaged copy are a miss: the script
//!   compiles and fresh bytes come back.

use otter_runtime::{Runtime, SourceInput};

const SCRIPT: &str = r#"
var calls = 0;
function helper(x) { calls++; return x * 2; }
function later() { return helper(21) + ':' + later.toString().length; }
later();
"#;

fn run(
    runtime: &mut Runtime,
    source: &str,
    specifier: &str,
    cache: Option<&[u8]>,
) -> (String, Option<Vec<u8>>) {
    let (result, fresh) = runtime
        .run_script_cached(SourceInput::from_javascript(source), specifier, cache)
        .expect("script runs");
    (result.completion_string().to_owned(), fresh)
}

#[test]
fn a_kept_cache_runs_the_script_without_compiling_it() {
    let mut first = Runtime::builder().build().expect("runtime");
    let (completion, bytes) = run(&mut first, SCRIPT, "https://example.test/a.js", None);
    let bytes = bytes.expect("a first run produces bytes");
    assert_eq!(completion, "42:71");

    let mut second = Runtime::builder().build().expect("runtime");
    let compiled = second.scripts_compiled();
    let (completion, fresh) = run(
        &mut second,
        SCRIPT,
        "https://example.test/a.js",
        Some(&bytes),
    );
    assert_eq!(completion, "42:71");
    assert!(fresh.is_none(), "the cache was used");
    assert_eq!(second.scripts_compiled(), compiled);
    // A function the cached script declared still compiles on first call.
    let lazily = second.lazy_function_bodies_compiled();
    let result = second
        .run_script(SourceInput::from_javascript("helper(5) + calls"), "<turn>")
        .expect("a turn");
    assert_eq!(result.completion_string(), "12");
    assert!(second.lazy_function_bodies_compiled() >= lazily);

    // Once its functions ran, the script's bytes carry their bodies, and a
    // run from those compiles none of them.
    let warm = first
        .script_code_cache("https://example.test/a.js")
        .expect("bodies compiled since the first run");
    assert!(
        first
            .script_code_cache("https://example.test/a.js")
            .is_none()
    );
    let mut third = Runtime::builder().build().expect("runtime");
    let lazily = third.lazy_function_bodies_compiled();
    let (completion, fresh) = run(&mut third, SCRIPT, "https://example.test/a.js", Some(&warm));
    assert_eq!(completion, "42:71");
    assert!(fresh.is_none());
    assert_eq!(third.lazy_function_bodies_compiled(), lazily);
    assert!(
        third
            .script_code_cache("https://example.test/a.js")
            .is_none()
    );
    // Another script run under the same address ends keeping bodies there.
    let other = "function other() { return helper(1); } other();";
    let (completion, _) = run(&mut third, other, "https://example.test/a.js", None);
    assert_eq!(completion, "2");
    assert!(
        third
            .script_code_cache("https://example.test/a.js")
            .is_none()
    );

    let stack = "function where() { return new Error('here').stack; } where();";
    let (_, bytes) = run(&mut first, stack, "https://example.test/a.js", None);
    let mut moved = Runtime::builder().build().expect("runtime");
    let (completion, fresh) = run(
        &mut moved,
        stack,
        "https://example.test/b.js?v=2",
        bytes.as_deref(),
    );
    assert!(
        fresh.is_none(),
        "another address of the same source is a hit"
    );
    assert!(
        completion.contains("b.js?v=2") && !completion.contains("a.js"),
        "{completion}"
    );
}

#[test]
fn bytes_that_do_not_match_are_a_miss() {
    let mut first = Runtime::builder().build().expect("runtime");
    let (_, bytes) = run(&mut first, SCRIPT, "https://example.test/a.js", None);
    let bytes = bytes.expect("bytes");
    let mut damaged = bytes.clone();
    let last = damaged.len() - 1;
    damaged[last] ^= 0xff;
    for (source, specifier, cache) in [
        (
            &*format!("{SCRIPT}\n// edited"),
            "https://example.test/a.js",
            &bytes[..],
        ),
        (SCRIPT, "https://example.test/a.js", &damaged[..]),
        (SCRIPT, "https://example.test/a.js", &b"not a cache"[..]),
    ] {
        let mut runtime = Runtime::builder().build().expect("runtime");
        let compiled = runtime.scripts_compiled();
        let (completion, fresh) = run(&mut runtime, source, specifier, Some(cache));
        assert_eq!(completion, "42:71");
        assert!(fresh.is_some(), "a miss produces fresh bytes");
        assert_eq!(runtime.scripts_compiled(), compiled + 1);
    }
}

/// A code-cache hook over a map.
#[derive(Default)]
struct MapCache {
    entries: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl otter_runtime::RuntimeCodeCacheHook for MapCache {
    fn load(&self, key: &str) -> Option<Vec<u8>> {
        self.entries.lock().unwrap().get(key).cloned()
    }

    fn store(&self, key: &str, bytes: Vec<u8>) {
        self.entries.lock().unwrap().insert(key.to_owned(), bytes);
    }
}

#[test]
fn a_large_eval_runs_again_from_the_hook_with_its_bodies() {
    let library = format!(
        "{}\nvar lib = {{ twice: function (x) {{ return x * 2; }} }}; lib.twice(1);\n//# sourceURL=https://example.test/lib.js",
        "/* padding */ ".repeat(2048)
    );
    let page = format!(
        "(0, eval)({}); lib.twice(21)",
        serde_json::to_string(&library).unwrap()
    );
    let cache = std::sync::Arc::new(MapCache::default());

    let mut first = Runtime::builder().build().expect("runtime");
    first.set_code_cache_hook(Some(cache.clone()));
    let lazily = first.lazy_function_bodies_compiled();
    let result = first
        .run_script(
            SourceInput::from_javascript(&page),
            "https://example.test/page.js",
        )
        .expect("page runs");
    assert_eq!(result.completion_string(), "42");
    assert_eq!(first.lazy_function_bodies_compiled(), lazily + 1);
    first.keep_eval_code_caches();
    assert_eq!(cache.entries.lock().unwrap().len(), 1);

    let mut second = Runtime::builder().build().expect("runtime");
    second.set_code_cache_hook(Some(cache.clone()));
    let lazily = second.lazy_function_bodies_compiled();
    let result = second
        .run_script(
            SourceInput::from_javascript(&page),
            "https://example.test/page.js",
        )
        .expect("page runs");
    assert_eq!(result.completion_string(), "42");
    // Nothing the evaluated library defines compiles again.
    assert_eq!(second.lazy_function_bodies_compiled(), lazily);
}

#[test]
fn a_script_prepared_on_another_thread_runs_as_a_cached_one() {
    let prepare = |cache: Option<Vec<u8>>| {
        std::thread::spawn(move || {
            otter_runtime::PreparedScript::prepare(
                SourceInput::from_javascript(SCRIPT),
                "https://example.test/a.js",
                cache.as_deref(),
            )
        })
        .join()
        .expect("prepared")
    };
    let mut runtime = Runtime::builder().build().expect("runtime");
    let compiled = runtime.scripts_compiled();
    let (result, fresh) = runtime.run_prepared_script(prepare(None)).expect("runs");
    assert_eq!(result.completion_string(), "42:71");
    assert_eq!(runtime.scripts_compiled(), compiled + 1);
    let mut again = Runtime::builder().build().expect("runtime");
    let compiled = again.scripts_compiled();
    let (result, fresh) = again.run_prepared_script(prepare(fresh)).expect("runs");
    assert_eq!(result.completion_string(), "42:71");
    assert!(fresh.is_none(), "the bytes were used");
    assert_eq!(again.scripts_compiled(), compiled);
}
