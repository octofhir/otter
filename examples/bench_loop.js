// Simple loop benchmark for JIT vs interpreter comparison
function fib(n) {
  if (n <= 1) return n;
  let a = 0, b = 1;
  for (let i = 2; i <= n; i++) {
    let tmp = a + b;
    a = b;
    b = tmp;
  }
  return b;
}

let sum = 0;
for (let i = 0; i < 100000; i++) {
  sum += fib(30);
}
console.log("result:", sum);
