
var Class = {
  create: function() {
    return function() { this.initialize.apply(this, arguments); };
  }
};
var Point = Class.create();
Point.prototype.initialize = function(x, y, z) {
  this.x = x;
  this.y = y;
  this.z = z === undefined ? 0 : z;
  this.n = arguments.length;
};
function build(i) { return new Point(i, i + 1); }
function sloppyMapped(a, b) {
  arguments[0] = a + b;
  return a + "," + arguments[0] + "," + arguments.length;
}
function strictUnmapped(a, b) {
  "use strict";
  arguments[0] = 99;
  return a + "," + arguments[0] + "," + arguments.length + "," + arguments[2];
}
function tagged(o, s) {
  // Heap values sit in the published window while the arguments object is
  // allocated; a moving collection must retarget them there.
  return arguments[0].k + arguments[1] + arguments.length + arguments[2].length;
}
function drive(rounds) {
  let acc = 0;
  let tail = "";
  for (let i = 0; i < rounds; i++) {
    const p = build(i);
    acc += p.x + p.y + p.z + p.n;
    tail = sloppyMapped(i, 1) + "|" + strictUnmapped(i, 2, 3) + "|" +
      tagged({ k: i }, "s" + i, [i, i]);
  }
  return acc + "|" + tail;
}
console.log(drive(256));
