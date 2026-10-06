function f3(a, b, c) { return a + b + c; }
const A3 = [1, 2, 3];
const fw = (...a) => f3(...a);
function g(a, b) { "use strict"; return arguments.length + arguments[0]; }
function gs(a, b) { return arguments.length + arguments[0]; }
function d() { const [a, b, c] = A3; return a + b + c; }
function sp() { return f3(...A3); }
function fo() { let s = 0; for (const x of A3) s += x; return s; }
console.log(fw(1, 2, 3), g(1, 2), gs(1, 2), d(), sp(), fo());
