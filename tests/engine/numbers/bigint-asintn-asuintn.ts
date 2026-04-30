/* otter-test:
name = "numbers: BigInt.asIntN / asUintN clip to N-bit lanes"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// asUintN — wraps modulo 2^N.
if (BigInt.asUintN(8, 0n) !== 0n) fail();
if (BigInt.asUintN(8, 256n) !== 0n) fail();
if (BigInt.asUintN(8, 257n) !== 1n) fail();
if (BigInt.asUintN(8, -1n) !== 255n) fail();
if (BigInt.asUintN(16, 65535n) !== 65535n) fail();
// asIntN — sign-extends.
if (BigInt.asIntN(8, 127n) !== 127n) fail();
if (BigInt.asIntN(8, 128n) !== -128n) fail();
if (BigInt.asIntN(8, 200n) !== -56n) fail();
if (BigInt.asIntN(8, -129n) !== 127n) fail();
// 64-bit lane example.
if (BigInt.asUintN(64, 18446744073709551616n) !== 0n) fail();
