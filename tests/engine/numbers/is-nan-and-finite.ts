/* otter-test:
name = "numbers: isNaN / isFinite (global coercing vs Number.* strict)"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// Global form coerces.
if (isNaN(NaN) !== true) fail();
if (isNaN("hello") !== true) fail();
if (isNaN(42) !== false) fail();
if (isFinite(1) !== true) fail();
if (isFinite("1") !== true) fail();
if (isFinite(NaN) !== false) fail();
if (isFinite(1 / 0) !== false) fail();

// Strict form does not coerce.
if (Number.isNaN(NaN) !== true) fail();
if (Number.isNaN("hello") !== false) fail();
if (Number.isFinite(1) !== true) fail();
if (Number.isFinite("1") !== false) fail();
if (Number.isInteger(3) !== true) fail();
if (Number.isInteger(3.5) !== false) fail();
if (Number.isInteger("3") !== false) fail();
if (Number.isSafeInteger(2 ** 53 - 1) !== true) fail();
if (Number.isSafeInteger(2 ** 53) !== false) fail();
