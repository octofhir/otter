//! `JSON.parse` keeps every character of a string that has an escape in it.
//!
//! A string whose first escape comes after a non-ASCII character took the
//! slow path with its prefix cast byte by byte, so `"— <"` came back as
//! `"â\u{80}\u{94} <"`: the UTF-8 of the dash read as Latin-1. Next.js pages
//! carry their props as JSON full of `<`, and The Verge's React
//! hydration failed on every such paragraph.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "https://site.test/run.js",
        )
        .expect("script runs")
        .completion_string()
        .to_owned()
}

#[test]
fn a_string_with_an_escape_keeps_the_characters_before_it() {
    let result = run(r#"
        const cases = [
            ['{"a":"— \\u003c"}', '— <'],
            ['{"a":"TÜV \\n x"}', 'TÜV \n x'],
            ['["😀\\té"]', '😀\té'],
            ['{"é\\u0041":1}', null],
        ];
        const out = [];
        for (const [text, expected] of cases) {
            const parsed = JSON.parse(text);
            const value = Array.isArray(parsed) ? parsed[0] : (expected === null ? Object.keys(parsed)[0] : parsed.a);
            out.push(value === (expected ?? 'éA'));
        }
        out.join(',')
    "#);
    assert_eq!(result, "true,true,true,true");
}
