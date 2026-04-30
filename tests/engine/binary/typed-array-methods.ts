/* otter-test:
name = "binary: TypedArray prototype methods (pure-functional surface)"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

// at()
const a = new Uint8Array([10, 20, 30, 40]);
if (a.at(0) !== 10) fail();
if (a.at(-1) !== 40) fail();
if (a.at(99) !== undefined) fail();

// indexOf / lastIndexOf / includes
if (a.indexOf(20) !== 1) fail();
if (a.indexOf(99) !== -1) fail();
if (a.lastIndexOf(30) !== 2) fail();
if (!a.includes(20)) fail();
if (a.includes(99)) fail();

// join + toString
if (a.join("-") !== "10-20-30-40") fail();
if (a.toString() !== "10,20,30,40") fail();

// slice — a fresh independent array.
const sliced = a.slice(1, 3);
if (sliced.length !== 2) fail();
if (sliced[0] !== 20) fail();
if (sliced[1] !== 30) fail();
sliced[0] = 99;
if (a[1] !== 20) fail();

// subarray — shares the underlying buffer.
const sub = a.subarray(1, 3);
if (sub.length !== 2) fail();
sub[0] = 99;
if (a[1] !== 99) fail();
a[1] = 20;

// fill
const f = new Uint8Array(5);
f.fill(7);
if (f[0] !== 7 || f[4] !== 7) fail();
f.fill(3, 1, 4);
if (f[0] !== 7 || f[1] !== 3 || f[3] !== 3 || f[4] !== 7) fail();

// copyWithin handles overlap.
const c = new Uint8Array([1, 2, 3, 4, 5]);
c.copyWithin(0, 3);
if (c[0] !== 4 || c[1] !== 5 || c[2] !== 3) fail();

// reverse
const r = new Uint8Array([1, 2, 3, 4]);
r.reverse();
if (r[0] !== 4 || r[3] !== 1) fail();

// toReversed / toSorted / with — all return fresh arrays.
const orig = new Uint8Array([3, 1, 2]);
const rev = orig.toReversed();
if (rev[0] !== 2 || rev[2] !== 3) fail();
if (orig[0] !== 3) fail();
const sorted = orig.toSorted();
if (sorted[0] !== 1 || sorted[2] !== 3) fail();
if (orig[0] !== 3) fail();
const replaced = orig.with(0, 99);
if (replaced[0] !== 99) fail();
if (orig[0] !== 3) fail();

// sort default (numeric).
const s = new Int32Array([3, 1, 4, 1, 5, 9, 2, 6]);
s.sort();
if (s[0] !== 1 || s[7] !== 9) fail();

// set() with array source.
const setArr = new Uint8Array(5);
setArr.set([10, 20, 30], 1);
if (setArr[0] !== 0 || setArr[1] !== 10 || setArr[3] !== 30) fail();

// set() with typedarray source — handles aliasing.
const aliased = new Uint8Array([1, 2, 3, 4, 5]);
aliased.set(aliased.subarray(0, 3), 2);
if (aliased[2] !== 1 || aliased[3] !== 2 || aliased[4] !== 3) fail();
