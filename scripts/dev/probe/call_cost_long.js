function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
let r = 0;
for (let i = 0; i < 300; i++) r += fib(27);
console.log(r);
