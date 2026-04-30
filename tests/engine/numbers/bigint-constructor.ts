/* otter-test:
name = "numbers: BigInt(value) constructor coerces inputs"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// Number → BigInt (only safe integers).
if (BigInt(42) !== 42n) fail();
if (BigInt(-7) !== -7n) fail();
if (BigInt(0) !== 0n) fail();
// Boolean → 0n / 1n.
if (BigInt(true) !== 1n) fail();
if (BigInt(false) !== 0n) fail();
// String — decimal / hex / binary / octal.
if (BigInt("1000000000000000000000").toString() !== "1000000000000000000000") fail();
if (BigInt("0xff") !== 255n) fail();
if (BigInt("0b1010") !== 10n) fail();
if (BigInt("0o17") !== 15n) fail();
// BigInt → BigInt (passes through).
if (BigInt(99n) !== 99n) fail();
