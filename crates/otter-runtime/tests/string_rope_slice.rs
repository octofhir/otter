//! Runtime regression coverage for slicing concatenated string ropes.
//!
//! # Contents
//! - Many small `slice()` results from one large rope-backed string.
//!
//! # Invariants
//! - Slicing a `Cons` string materialises only the requested span, not the
//!   whole source rope for every substring.
//!
//! # See also
//! - `otter-vm::string::gc_body::slice_string_body`

use otter_runtime::{Runtime, SourceInput};

#[test]
fn slicing_many_small_fields_from_rope_stays_linear() {
    let mut rt = Runtime::builder()
        .max_heap_bytes(64 * 1024 * 1024)
        .build()
        .expect("runtime");

    let completion = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
                function make(count) {
                    let str = "";
                    if (count === 0) return str;
                    str += "0=0";
                    for (let i = 1; i < count; i++) {
                        const n = i.toString(36);
                        str += "&" + n + "=" + n;
                    }
                    return str;
                }

                const qs = make(10000);
                const obj = { __proto__: null };
                let last = 0;
                let eq = 0;
                for (let i = 0; i < qs.length; i++) {
                    const code = qs.charCodeAt(i);
                    if (code === 61) {
                        eq = i;
                    } else if (code === 38) {
                        obj[qs.slice(last, eq)] = qs.slice(eq + 1, i);
                        last = i + 1;
                    }
                }
                obj[qs.slice(last, eq)] = qs.slice(eq + 1);
                Object.keys(obj).length + "|" + obj["0"] + "|" + obj["7pr"];
                "#,
            ),
            "<string-rope-slice>",
        )
        .expect("script")
        .completion_string()
        .to_string();

    assert_eq!(completion, "10000|0|7pr");
}
