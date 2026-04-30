/* otter-test:
name = "iterator: map / filter chained lazily"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// map produces a new iterator with the transform applied per step.
const mapped = Iterator.from([1, 2, 3]).map((x) => x * 2).toArray();
if (mapped.length !== 3) fail();
if (mapped[0] !== 2 || mapped[2] !== 6) fail();

// filter skips elements where the predicate returns falsey.
const even = Iterator.from([1, 2, 3, 4, 5, 6]).filter((x) => x % 2 === 0).toArray();
if (even.length !== 3) fail();
if (even[0] !== 2 || even[2] !== 6) fail();

// Chain map and filter — verify pipelining is left-to-right.
const chained = Iterator.from([1, 2, 3, 4])
    .map((x) => x + 10)
    .filter((x) => x > 12)
    .toArray();
if (chained.length !== 2) fail();
if (chained[0] !== 13 || chained[1] !== 14) fail();
