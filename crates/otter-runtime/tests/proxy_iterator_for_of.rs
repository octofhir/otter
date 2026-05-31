//! §7.4.x — `for…of` must drive an iterator whose `next` is reached
//! through a Proxy (or an accessor), resolving it via the ordinary
//! [[Get]] ladder rather than a plain own-property read.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<proxy-iter>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn for_of_over_proxy_iterator() {
    let out = run(r#"
        var inner = { next() { return { value: 23, done: false }; } };
        var proxied = new Proxy(inner, { get(t, n) { return t[n]; } });
        var iterable = { [Symbol.iterator]() { return proxied; } };
        var n = 0, last = 0;
        for (var x of iterable) { last = x; n++; if (n > 2) break; }
        n + ',' + last;
    "#);
    assert_eq!(out, "3,23");
}

#[test]
fn plain_user_iterator_still_works() {
    assert_eq!(
        run(
            "var it={[Symbol.iterator](){var i=0;return{next(){return i<3?{value:i++,done:false}:{done:true};}};}}; var s=0; for(var x of it)s+=x; String(s);"
        ),
        "3"
    );
}
