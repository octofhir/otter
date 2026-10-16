//! Eval code named with `//# sourceURL=` is reported under that name.
//!
//! Script loaders run fetched text through indirect `eval` and name it with
//! the comment; without it every frame of such code was `<eval>`, which
//! said nothing about which of a page's scripts had thrown.

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
fn eval_code_takes_the_name_its_comment_gives() {
    let stack = run(r#"
        let stack = '';
        try {
            (0, eval)('function thrower() { throw new Error("x"); }\nthrower();\n//# sourceURL=https://cdn.test/app.js');
        } catch (error) { stack = error.stack; }
        stack
    "#);
    assert!(
        stack.contains("at thrower (https://cdn.test/app.js:1:"),
        "{stack}"
    );
}

#[test]
fn eval_code_without_a_usable_name_stays_eval() {
    for source in [
        "throw new Error('x')",
        "throw new Error('x')\n//# sourceURL=",
        "throw new Error('x')\n//# sourceURL=has space.js",
    ] {
        let stack = run(&format!(
            "let stack = ''; try {{ (0, eval)({source:?}); }} catch (error) {{ stack = error.stack; }} stack"
        ));
        assert!(stack.contains("<eval>"), "{source}: {stack}");
    }
}
