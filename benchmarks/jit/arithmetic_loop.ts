// JIT benchmark: int32/f64 arithmetic in tight loop
// Measures: typed arithmetic fast paths, overflow handling, loop back-edge efficiency

function benchInt32Add(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum = (sum + i) | 0; // force int32
  }
  return sum;
}

function benchInt32Mul(n: number): number {
  let result = 1;
  for (let i = 1; i <= n; i++) {
    result = (result * i) | 0;
    if (result > 1_000_000) result = 1;
  }
  return result;
}

function benchFloat64(n: number): number {
  let sum = 0.0;
  for (let i = 0; i < n; i++) {
    sum += i * 1.1;
    sum -= i * 0.1;
  }
  return sum;
}

function benchMixed(n: number): number {
  let intSum = 0;
  let floatSum = 0.0;
  for (let i = 0; i < n; i++) {
    intSum = (intSum + i) | 0;
    floatSum += intSum * 0.5;
  }
  return floatSum;
}

const N = 1_000_000;
const ITERS = 50;

const start = Date.now();
for (let iter = 0; iter < ITERS; iter++) {
  benchInt32Add(N);
  benchInt32Mul(N);
  benchFloat64(N);
  benchMixed(N);
}
const elapsed = Date.now() - start;
console.log(`arithmetic_loop: ${elapsed}ms (${ITERS} iterations, N=${N})`);
