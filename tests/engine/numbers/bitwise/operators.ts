/* otter-test:
name = "bitwise: & | ^ << >> >>> ~ produce spec results"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ((5 & 3) !== 1) fail();
if ((5 | 3) !== 7) fail();
if ((5 ^ 3) !== 6) fail();
if ((1 << 3) !== 8) fail();
if ((-1 >> 1) !== -1) fail();
// (-1 >>> 0) === 4294967295 — exceeds i32::MAX, lands as a Double.
if ((-1 >>> 0) !== 4294967295) fail();
if (~0 !== -1) fail();
// Shift count is masked to its low 5 bits.
if ((1 << 33) !== 2) fail();
// Floats coerce through ToInt32 first.
if ((3.7 | 0) !== 3) fail();
