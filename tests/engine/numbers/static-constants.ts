/* otter-test:
name = "numbers: Number static constants"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
if (Number.MAX_SAFE_INTEGER !== 9007199254740991) fail();
if (Number.MIN_SAFE_INTEGER !== -9007199254740991) fail();
if (Number.POSITIVE_INFINITY !== Infinity) fail();
if (Number.NEGATIVE_INFINITY !== -Infinity) fail();
if (Number.MAX_VALUE > 1e300 !== true) fail();
if (Number.MIN_VALUE > 0 !== true) fail();
if (Number.MIN_VALUE < 1e-300 !== true) fail();
if (Number.EPSILON > 0 !== true) fail();
if (Number.EPSILON < 1e-10 !== true) fail();
// Number.NaN is NaN — must compare unequal to itself.
if (Number.NaN === Number.NaN) fail();
if (Number.isNaN(Number.NaN) !== true) fail();
