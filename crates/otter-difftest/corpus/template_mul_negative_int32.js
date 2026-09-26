// A template-tier int32 multiply forms its product in 64 bits for the
// overflow test. A negative product must still box canonically (upper word
// clear): consumers with exact int32 tag tests — here a Float32Array store —
// otherwise classify it as a double and store garbage.
var F = new Float32Array(8);
var I = new Int32Array(8);
var g = 1, minusOne = -1, three = 3;
function mulStore(i) { F[i] = minusOne * g; I[i] = minusOne * three; }
function readMul(i) { var v = F[i]; return v * g; }
var out = [];
for (var k = 0; k < 3000; k++) {
  mulStore(1);
  F[2] = -1;
  var r = readMul(2);
  if (k % 1000 === 0) out.push(F[1], I[1], r, minusOne * g, (minusOne * g) | 0);
}
JSON.stringify(out);
