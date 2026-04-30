/* otter-test:
name = "iterator: Iterator.from + toArray drains every iterable shape"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// Array source.
const a = Iterator.from([1, 2, 3]).toArray();
if (a.length !== 3) fail();
if (a[0] !== 1 || a[2] !== 3) fail();

// String source — yields code units / code points.
const s = Iterator.from("abc").toArray();
if (s.length !== 3) fail();
if (s[0] !== "a" || s[2] !== "c") fail();

// Set source.
const set = new Set([10, 20, 30]);
const r = Iterator.from(set).toArray();
if (r.length !== 3) fail();

// Already-an-Iterator passes through.
const it = Iterator.from([7, 8]);
const echoed = Iterator.from(it).toArray();
if (echoed.length !== 2) fail();

// Empty source.
if (Iterator.from([]).toArray().length !== 0) fail();
