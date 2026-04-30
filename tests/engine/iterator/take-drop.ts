/* otter-test:
name = "iterator: take / drop honour boundaries and short-circuit"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// take(n) yields at most n elements.
const t = Iterator.from([1, 2, 3, 4, 5]).take(3).toArray();
if (t.length !== 3) fail();
if (t[0] !== 1 || t[2] !== 3) fail();

// take(0) is empty.
if (Iterator.from([1, 2, 3]).take(0).toArray().length !== 0) fail();

// take(n) where n > length stops at the source's end.
const tShort = Iterator.from([1, 2]).take(99).toArray();
if (tShort.length !== 2) fail();

// drop(n) discards the first n elements.
const d = Iterator.from([1, 2, 3, 4, 5]).drop(2).toArray();
if (d.length !== 3) fail();
if (d[0] !== 3 || d[2] !== 5) fail();

// drop(0) yields the entire source.
const d0 = Iterator.from([7, 8]).drop(0).toArray();
if (d0.length !== 2) fail();

// drop(n) where n >= length yields empty.
if (Iterator.from([1, 2]).drop(99).toArray().length !== 0) fail();

// take + drop chains short-circuit eagerly when combined.
const td = Iterator.from([1, 2, 3, 4, 5, 6]).drop(2).take(2).toArray();
if (td.length !== 2) fail();
if (td[0] !== 3 || td[1] !== 4) fail();
