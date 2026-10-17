//! A script an embedder ran once runs again from the bytes it kept,
//! without compiling.
//!
//! # Invariants
//!
//! - The first run of a script hands back bytes; a run given them compiles
//!   nothing and behaves the same, lazily compiled functions included.
//! - Bytes for another source, another specifier or a damaged copy are a
//!   miss: the script compiles and fresh bytes come back.

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
        (SCRIPT, "https://example.test/b.js", &bytes[..]),
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
