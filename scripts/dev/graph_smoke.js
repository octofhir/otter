function add(a, b) { return a + b; }
function sum(n) { let s = 0; for (let i = 0; i < n; i++) s = s + i; return s; }
function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
function point(o) { return o.x + o.y; }
let total = 0;
for (let k = 0; k < 20000; k++) { total = add(total, k); }
console.log("add", total);
let s = 0;
for (let k = 0; k < 2000; k++) s = sum(100);
console.log("sum", s);
console.log("fib", fib(25));
let p = 0;
for (let k = 0; k < 20000; k++) p = point({ x: k, y: 1 });
console.log("point", p);
