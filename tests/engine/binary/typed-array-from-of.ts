/* otter-test:
name = "binary: TypedArray.from + .of statics"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

// §23.2.2.2 `T.of(...items)`.
const a = Uint8Array.of(1, 2, 3, 4);
if (a.length !== 4) fail();
if (a[0] !== 1) fail();
if (a[3] !== 4) fail();

const big = BigInt64Array.of(1n, 2n, 3n);
if (big.length !== 3) fail();
if (big[0] !== 1n) fail();

// §23.2.2.1 `T.from(arrayLike)`.
const b = Int32Array.from([10, 20, 30]);
if (b.length !== 3) fail();
if (b[1] !== 20) fail();

const c = Uint8Array.from(new Uint16Array([1, 2, 3]));
if (c.length !== 3) fail();
if (c[2] !== 3) fail();

// from object with `length` and indexed properties.
const d = Uint8Array.from({ length: 2, "0": 5, "1": 6 });
if (d.length !== 2) fail();
if (d[0] !== 5 || d[1] !== 6) fail();
