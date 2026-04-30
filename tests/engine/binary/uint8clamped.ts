/* otter-test:
name = "binary: Uint8ClampedArray clamping per §6.1.6.1"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

const a = new Uint8ClampedArray(8);
a[0] = 300;
a[1] = -1;
a[2] = NaN;
a[3] = Infinity;
a[4] = -Infinity;
a[5] = 100.5;  // round half to even → 100
a[6] = 101.5;  // round half to even → 102
a[7] = 200.7;
if (a[0] !== 255) fail();
if (a[1] !== 0) fail();
if (a[2] !== 0) fail();
if (a[3] !== 255) fail();
if (a[4] !== 0) fail();
if (a[5] !== 100) fail();
if (a[6] !== 102) fail();
if (a[7] !== 201) fail();
