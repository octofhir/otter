/* otter-test:
name = "regexp: named-capture groups expose .groups + index/input"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const r = /(?<year>\d{4})-(?<month>\d{2})/.exec("xx 2026-04 yy");
if (r === null) fail();
if (r.length !== 3) fail();
if (r[0] !== "2026-04") fail();
if (r[1] !== "2026") fail();
if (r[2] !== "04") fail();
if (r.index !== 3) fail();
if (r.input !== "xx 2026-04 yy") fail();
if (typeof r.groups !== "object") fail();
if (r.groups.year !== "2026") fail();
if (r.groups.month !== "04") fail();

// No named groups → groups is undefined.
const r2 = /\d+/.exec("hello 42");
if (r2 === null) fail();
if (r2.groups !== undefined) fail();
if (r2.index !== 6) fail();
if (r2.input !== "hello 42") fail();

// Optional named group that did not match → undefined entry.
const r3 = /(?<a>x)|(?<b>y)/.exec("y");
if (r3 === null) fail();
if (r3.groups.a !== undefined) fail();
if (r3.groups.b !== "y") fail();
