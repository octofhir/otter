// Nested field merges of a nonescaping constructor receiver must use live SSA
// node ids, including a value merged once and then changed by another branch.
"use strict";
function Point(x) { this.x = x; this.y = 0.25; }
var seed = new Point(0.25);
function build(x) {
  var p = new Point(x);
  if (x < 1) p.x = x + 0.5;
  if (x > 1.5) p.x = x + 0.75;
  return p.x + p.y;
}
var sum = 0;
for (var i = 0; i < 32000; i++) sum += build((i % 8) * 0.25);
if (sum !== 47000) throw new Error("nested field merge: " + sum);
console.log("nested-fields", sum, seed.x);
