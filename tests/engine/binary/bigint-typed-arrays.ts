/* otter-test:
name = "binary: BigInt64 / BigUint64 typed arrays"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

const i = new BigInt64Array(3);
i[0] = 9n;
i[1] = -1n;
i[2] = 9223372036854775807n; // 2^63 - 1
if (i[0] !== 9n) fail();
if (i[1] !== -1n) fail();
if (i[2] !== 9223372036854775807n) fail();

const u = new BigUint64Array(2);
u[0] = 18446744073709551615n; // 2^64 - 1
u[1] = -1n; // wraps mod 2^64
if (u[0] !== 18446744073709551615n) fail();
if (u[1] !== 18446744073709551615n) fail();

// Number stores are TypeError.
let threw = false;
try { i[0] = 1; } catch (e) { threw = true; }
if (!threw) fail();

// fill with BigInt.
const f = new BigInt64Array(3);
f.fill(7n);
if (f[0] !== 7n || f[2] !== 7n) fail();

// Sort BigInt arrays.
const s = new BigInt64Array([3n, 1n, 2n]);
s.sort();
if (s[0] !== 1n || s[2] !== 3n) fail();
