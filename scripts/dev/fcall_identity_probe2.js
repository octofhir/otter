function AST(t) { this.t = t; }
function f(o, i) { AST.call(o, i); return o.t; }
function run(n) { var s = 0; var o = {}; for (var i = 0; i < n; i++) s += f(o, i); return s; }
var total = 0;
for (var r = 0; r < 20; r++) total += run(20000);
console.log(total);
