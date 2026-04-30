/* otter-test:
name = "regexp: d flag exposes .indices alongside the match array"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const r = /(?<word>\w+)/d.exec("hello world");
if (r === null) fail();
if (r[0] !== "hello") fail();
if (r.index !== 0) fail();
// Bare `flags` accessor must include `d` and follow canonical order.
if (/abc/dgi.flags !== "dgi") fail();
if (/abc/dgi.hasIndices !== true) fail();
if (/abc/g.hasIndices !== false) fail();

// `indices` mirrors captures + named groups companion.
const idx = r.indices;
if (idx === undefined) fail();
if (idx.length !== 2) fail();
if (idx[0][0] !== 0 || idx[0][1] !== 5) fail();
if (idx[1][0] !== 0 || idx[1][1] !== 5) fail();
if (typeof idx.groups !== "object") fail();
if (idx.groups.word[0] !== 0 || idx.groups.word[1] !== 5) fail();

// Without the `d` flag there is no `indices` companion.
const r2 = /\w+/.exec("hello");
if (r2.indices !== undefined) fail();
