/* otter-test:
name = "iterator: reduce / forEach drain the source eagerly"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// reduce with an initial value.
const sum = Iterator.from([1, 2, 3, 4]).reduce((acc, v) => acc + v, 0);
if (sum !== 10) fail();

// reduce without initial — first element seeds the accumulator.
const sumNoInit = Iterator.from([5, 10, 15]).reduce((acc, v) => acc + v);
if (sumNoInit !== 30) fail();

// reduce on a chained pipeline.
const chained = Iterator.from([1, 2, 3, 4, 5])
    .filter((x) => x % 2 === 1)
    .map((x) => x * x)
    .reduce((acc, v) => acc + v, 0);
if (chained !== 35) fail();

// forEach observes every element in source order.
const seen = [];
Iterator.from([10, 20, 30]).forEach((v) => {
    seen.push(v);
});
if (seen.length !== 3) fail();
if (seen[0] !== 10 || seen[2] !== 30) fail();

// forEach returns undefined.
const ret = Iterator.from([1]).forEach(() => {});
if (ret !== undefined) fail();
