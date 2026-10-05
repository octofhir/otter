function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
let s = 0;
for (let i = 0; i < 5; i++) s += fib(30);
console.log(s);
