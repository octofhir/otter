//! An error a builtin throws records the script frames that called it.
//!
//! Pages report errors by their `stack`; one thrown by `Array.prototype.push`
//! on a frozen array came with the message alone, which left nothing to say
//! where Notion's bundle pushed.

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
fn a_builtins_error_names_the_frames_that_called_it() {
    let result = run(r#"
        function pushOnto(list) { list.push(1); }
        function caller() { pushOnto(Object.freeze([])); }
        let stack = '';
        try { caller(); } catch (error) { stack = error.stack; }
        const lines = stack.split('\n');
        [lines[0].startsWith('TypeError: '), /at pushOnto \(.*run\.js:2:/.test(lines[1]), /at caller /.test(lines[2])].join()
    "#);
    assert_eq!(result, "true,true,true");
}

#[test]
fn a_thrown_error_keeps_its_own_frames_across_a_builtin() {
    let result = run(r#"
        function make() { return new Error('made'); }
        const error = make();
        let caught;
        try { [1].forEach(() => { throw error; }); } catch (e) { caught = e; }
        [caught === error, /at make /.test(caught.stack.split('\n')[1])].join()
    "#);
    assert_eq!(result, "true,true");
}
