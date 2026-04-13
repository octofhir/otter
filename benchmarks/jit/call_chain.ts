// JIT benchmark: monomorphic JS-to-JS call chains
// Measures: call dispatch overhead, direct call fast path, return value handling

function readPositiveIntArg(index: number, fallback: number): number {
  const raw = process.argv[index];
  if (raw === undefined) return fallback;
  const parsed = Number.parseInt(raw, 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

function add(a: number, b: number): number {
  return a + b;
}

function square(x: number): number {
  return x * x;
}

function identity(x: number): number {
  return x;
}

function compose2(x: number): number {
  return add(square(x), identity(x));
}

function benchDirectCall(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum += add(i, 1);
  }
  return sum;
}

function benchCallChain(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum += compose2(i);
  }
  return sum;
}

// Recursive fibonacci — tests call depth and return
function fib(n: number): number {
  if (n <= 1) return n;
  return fib(n - 1) + fib(n - 2);
}

function benchFib(n: number, depth: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum += fib(depth);
  }
  return sum;
}

// Method call on object (stable receiver shape)
const calculator = {
  value: 0,
  add(x: number): number {
    this.value += x;
    return this.value;
  },
  reset(): void {
    this.value = 0;
  },
};

function benchMethodCall(n: number): number {
  calculator.reset();
  for (let i = 0; i < n; i++) {
    calculator.add(i);
  }
  return calculator.value;
}

const N = readPositiveIntArg(2, 1_000_000);
const ITERS = readPositiveIntArg(3, 50);
const FIB_ITERS = readPositiveIntArg(4, 100);
const FIB_DEPTH = readPositiveIntArg(5, 20);

const start = Date.now();
for (let iter = 0; iter < ITERS; iter++) {
  benchDirectCall(N);
  benchCallChain(N);
  benchFib(FIB_ITERS, FIB_DEPTH);
  benchMethodCall(N);
}
const elapsed = Date.now() - start;
console.log(
  `call_chain: ${elapsed}ms (${ITERS} iterations, N=${N}, fibIters=${FIB_ITERS}, fibDepth=${FIB_DEPTH})`,
);
