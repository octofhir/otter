// Hot loop over top-level `var` / function globals, as Octane code reads
// its constructors and constants.
var Strength = { REQUIRED: 0, STRONG: 1 };
var COUNT = 3;
function Point(x) { this.x = x; }
function work(n) {
  var s = 0;
  for (var i = 0; i < n; i++) {
    s = (s + Strength.STRONG + COUNT) | 0;
    if (i % 1000 === 0) s = (s + new Point(i).x) | 0;
  }
  return s;
}
var t0 = Date.now();
var r = 0;
for (var k = 0; k < 20; k++) r = (r + work(5000000)) | 0;
console.log(r, Date.now() - t0, "ms");
