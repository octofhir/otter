/* otter-test:
name = "numbers: toExponential / toPrecision / valueOf"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// valueOf — returns the receiver.
let n = 1.75;
if (n.valueOf() !== 1.75) fail();

// toExponential.
if ((1500).toExponential(2) !== "1.50e+3") fail();
if ((0.0001234).toExponential(3) !== "1.234e-4") fail();
if ((123).toExponential(0) !== "1e+2") fail();
// Default (no arg) keeps full precision.
if ((1500).toExponential().indexOf("e+3") < 0) fail();

// toPrecision.
if ((123.456).toPrecision(4) !== "123.5") fail();
if ((0.000123).toPrecision(2) !== "0.00012") fail();
if ((1234567).toPrecision(3) !== "1.23e+6") fail();
if ((1).toPrecision(5) !== "1.0000") fail();
// No-arg form behaves as ToString.
if ((1.5).toPrecision() !== "1.5") fail();

// NaN / Infinity edge cases.
if (Number.NaN.toExponential(2) !== "NaN") fail();
if (Number.POSITIVE_INFINITY.toExponential(2) !== "Infinity") fail();
if (Number.NEGATIVE_INFINITY.toPrecision(3) !== "-Infinity") fail();
