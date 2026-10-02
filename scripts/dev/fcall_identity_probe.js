var Span = function Span() { this.minChar = -1; };
function AST(t) { Span.call(this); this.t = t; }
function Sub(x) { AST.call(this, x); this.x = x; }
function run(n) { var s = 0; for (var i = 0; i < n; i++) { var o = new Sub(i); s += o.t + o.minChar; } return s; }
var total = 0;
for (var r = 0; r < 20; r++) total += run(20000);
console.log(total);
