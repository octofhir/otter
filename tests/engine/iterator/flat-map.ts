/* otter-test:
name = "iterator: flatMap flattens arrays + iterators"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// Mapper returns arrays — flattened one level.
const a = Iterator.from([1, 2, 3]).flatMap((x) => [x, x * 10]).toArray();
if (a.length !== 6) fail();
if (a[0] !== 1 || a[1] !== 10) fail();
if (a[4] !== 3 || a[5] !== 30) fail();

// Mapper returns iterators — drained inline.
const b = Iterator.from([1, 2])
    .flatMap((x) => Iterator.from([x, x + 100]))
    .toArray();
if (b.length !== 4) fail();
if (b[0] !== 1 || b[1] !== 101 || b[3] !== 102) fail();

// Empty inner arrays just contribute nothing.
const c = Iterator.from([1, 2, 3]).flatMap((x) => (x === 2 ? [] : [x])).toArray();
if (c.length !== 2) fail();
if (c[0] !== 1 || c[1] !== 3) fail();
