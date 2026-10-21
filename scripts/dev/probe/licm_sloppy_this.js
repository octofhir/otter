// A sloppy function called without a receiver binds globalThis; a loop over
// `this.n` must see the bound object, not the undefined it was called with.
globalThis.n = 5;
function f() {
  var s = 0;
  for (var i = 0; i < this.n; i++) s += i;
  return s;
}
var total = 0;
for (var k = 0; k < 200000; k++) total += f();
console.log(total);
