function sum(a, b, c, d) { return a + b + c + d; }
function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
let value = 0;
for (let i = 0; i < 100; i++) value = sum(i, 1, 2, 3);
JSON.stringify({ value, fib: fib(10) });
