// Benchmark: measures overhead of JIT hot function detection + failed compilation attempts
// Functions called >1000 times trigger hot detection and JIT compile attempt (when enabled)

function hotAdd(a, b) {
  return a + b;
}

function hotFib(n) {
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
for (let i = 0; i < 50000; i++) {
  sum = hotAdd(sum, i);
}

let fsum = 0;
for (let i = 0; i < 5000; i++) {
  fsum += hotFib(20);
}

console.log("add result:", sum);
console.log("fib result:", fsum);
