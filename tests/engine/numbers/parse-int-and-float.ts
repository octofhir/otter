/* otter-test:
name = "numbers: parseInt / parseFloat as global + Number.<name>"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// Decimal.
if (parseInt("42") !== 42) fail();
if (parseInt("  -7  ") !== -7) fail();
// Hex auto-detect.
if (parseInt("0xff") !== 255) fail();
if (parseInt("ff", 16) !== 255) fail();
// Stops at first non-digit.
if (parseInt("12abc") !== 12) fail();
// NaN: returned by parseInt on a non-numeric string.
if (Number.isNaN(parseInt("abc")) !== true) fail();

if (parseFloat("3.14") !== 3.14) fail();
if (parseFloat("-1.5e2") !== -150) fail();
if (Number.isNaN(parseFloat("abc")) !== true) fail();

// Number.parseInt / parseFloat are aliases of the global form.
if (Number.parseInt("100") !== 100) fail();
if (Number.parseFloat("2.5") !== 2.5) fail();
