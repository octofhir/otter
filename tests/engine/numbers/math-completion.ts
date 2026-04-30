/* otter-test:
name = "numbers: Math constants + transcendental + integer helpers"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
function approx(a: number, b: number): boolean {
  return Math.abs(a - b) < 1e-9;
}

// Constants.
if (!approx(Math.LN2, 0.6931471805599453)) fail();
if (!approx(Math.LN10, 2.302585092994046)) fail();
if (!approx(Math.LOG2E, 1.4426950408889634)) fail();
if (!approx(Math.LOG10E, 0.4342944819032518)) fail();
if (!approx(Math.SQRT2, 1.4142135623730951)) fail();
if (!approx(Math.SQRT1_2, 0.7071067811865476)) fail();

// Transcendentals.
if (Math.log(1) !== 0) fail();
if (!approx(Math.log(Math.E), 1)) fail();
if (Math.log2(8) !== 3) fail();
if (Math.log10(1000) !== 3) fail();
if (Math.exp(0) !== 1) fail();
if (!approx(Math.exp(1), Math.E)) fail();
if (Math.sin(0) !== 0) fail();
if (Math.cos(0) !== 1) fail();
if (!approx(Math.atan2(1, 1), Math.PI / 4)) fail();
if (!approx(Math.cbrt(27), 3)) fail();
if (!approx(Math.cbrt(-8), -2)) fail();
if (!approx(Math.hypot(3, 4), 5)) fail();
if (Math.hypot() !== 0) fail();

// Sign / clz32 / imul.
if (Math.sign(-7) !== -1) fail();
if (Math.sign(0) !== 0) fail();
if (Math.sign(99) !== 1) fail();
if (Math.clz32(1) !== 31) fail();
if (Math.clz32(0) !== 32) fail();
if (Math.imul(2, 4) !== 8) fail();
if (Math.imul(-1, 5) !== -5) fail();

// random — bounded.
let r = Math.random();
if (r < 0 || r >= 1) fail();
let r2 = Math.random();
if (r2 < 0 || r2 >= 1) fail();
