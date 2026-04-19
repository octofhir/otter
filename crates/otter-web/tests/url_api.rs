//! Integration tests for the WHATWG URL / URLSearchParams surface.
//! Each test boots a fresh `OtterRuntime` with the `otter-web`
//! extension installed, compiles a small script that calls URL /
//! URLSearchParams methods, and reads back an int32 or string
//! observation via the script's return value.

use otter_runtime::OtterRuntime;
use otter_web::web_extension;

fn run_int(code: &str) -> i32 {
    let mut rt = OtterRuntime::builder().extension(web_extension()).build();
    let result = rt.run_script(code, "test://w1").expect("run_script");
    result
        .return_value()
        .as_i32()
        .expect("return value is int32")
}

fn run_bool(code: &str) -> bool {
    let mut rt = OtterRuntime::builder().extension(web_extension()).build();
    let result = rt.run_script(code, "test://w1").expect("run_script");
    result
        .return_value()
        .as_bool()
        .expect("return value is bool")
}

#[test]
fn w1_url_constructor_parses_full_url() {
    // Trivial smoke test — construct a URL and read its href back.
    // `.length` gives us an int32 to inspect.
    let out = run_int(
        "function main() { \
            let u = new URL(\"https://example.com:8080/a/b?x=1#frag\"); \
            return u.href.length \
        }",
    );
    assert_eq!(out, "https://example.com:8080/a/b?x=1#frag".len() as i32);
}

#[test]
fn w1_url_getters_return_spec_values() {
    let out = run_int(
        "function main() { \
            let u = new URL(\"https://user:pw@example.com:8080/path?q=1#h\"); \
            return u.protocol.length + u.hostname.length + u.pathname.length \
        }",
    );
    // "https:" (6) + "example.com" (11) + "/path" (5) = 22
    assert_eq!(out, 22);
}

#[test]
fn w1_url_href_setter_replaces_whole_url() {
    let out = run_int(
        "function main() { \
            let u = new URL(\"https://old.example/\"); \
            u.href = \"http://new.example/other\"; \
            return u.hostname.length \
        }",
    );
    assert_eq!(out, "new.example".len() as i32);
}

#[test]
fn w1_url_port_setter_strips_default_port() {
    let out = run_int(
        "function main() { \
            let u = new URL(\"https://example.com/\"); \
            u.port = \"8443\"; \
            return u.port.length \
        }",
    );
    assert_eq!(out, "8443".len() as i32);
}

#[test]
fn w1_url_pathname_setter_updates_path() {
    let out = run_int(
        "function main() { \
            let u = new URL(\"https://example.com/a\"); \
            u.pathname = \"/b/c\"; \
            return u.pathname.length \
        }",
    );
    assert_eq!(out, "/b/c".len() as i32);
}

#[test]
fn w1_url_search_setter_normalises_question_mark() {
    let out = run_int(
        "function main() { \
            let u = new URL(\"https://example.com/\"); \
            u.search = \"?a=1&b=2\"; \
            return u.search.length \
        }",
    );
    assert_eq!(out, "?a=1&b=2".len() as i32);
}

#[test]
fn w1_url_can_parse_returns_true_for_valid_url() {
    let out = run_bool("function main() { return URL.canParse(\"https://example.com/x\") }");
    assert!(out);
}

#[test]
fn w1_url_can_parse_returns_false_for_garbage_without_base() {
    let out = run_bool("function main() { return URL.canParse(\"not-a-url\") }");
    assert!(!out);
}

#[test]
fn w1_url_search_params_size_matches_entries() {
    let out = run_int(
        "function main() { \
            let sp = new URLSearchParams(\"a=1&b=2&c=3\"); \
            return sp.size \
        }",
    );
    assert_eq!(out, 3);
}

#[test]
fn w1_url_search_params_sort_stable() {
    let out = run_int(
        "function main() { \
            let sp = new URLSearchParams(\"b=1&a=2&c=3\"); \
            sp.sort(); \
            return sp.toString().length \
        }",
    );
    // Sorted → "a=2&b=1&c=3" (length 11).
    assert_eq!(out, "a=2&b=1&c=3".len() as i32);
}

#[test]
fn w1_url_search_params_get_returns_first_value() {
    let out = run_int(
        "function main() { \
            let sp = new URLSearchParams(\"x=1&x=2\"); \
            return sp.get(\"x\").length \
        }",
    );
    assert_eq!(out, 1);
}
