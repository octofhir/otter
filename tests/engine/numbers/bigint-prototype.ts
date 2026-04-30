/* otter-test:
name = "numbers: BigInt.prototype.toString(radix) + valueOf"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let x = 255n;
if (x.toString() !== "255") fail();
if (x.toString(16) !== "ff") fail();
if (x.toString(2) !== "11111111") fail();
let neg = -42n;
if (neg.toString(10) !== "-42") fail();
if (neg.toString(16) !== "-2a") fail();
// valueOf returns the receiver.
if ((42n).valueOf() !== 42n) fail();
// Huge value — radix conversion.
let big = 1208925819614629174706176n; // 2 ** 80
if (big.toString(16) !== "100000000000000000000") fail();
