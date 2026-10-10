// OSR enters an inner loop; the enclosing loop then runs again along its
// back edge, where values the forward path loaded before it (here `this`
// and its field) were never computed.
function W() { this.g = { x: 1 }; this.n = 4; }
W.prototype.run = function (iters) {
  var first = this.g.x;
  var s = 0;
  for (var o = 0; o < this.n; o++) {
    for (var i = 0; i < iters; i++) s += i & 1;
    s += this.g.x + first;
  }
  return s;
};
console.log(new W().run(400000));
