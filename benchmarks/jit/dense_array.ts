// JIT benchmark: dense array element access, push/pop, length
// Measures: bounds checking, dense element fast path, array length inline

function benchArrayRead(arr: number[], n: number): number {
  let sum = 0;
  const len = arr.length;
  for (let i = 0; i < n; i++) {
    sum += arr[i % len];
  }
  return sum;
}

function benchArrayWrite(arr: number[], n: number): void {
  const len = arr.length;
  for (let i = 0; i < n; i++) {
    arr[i % len] = i;
  }
}

function benchArrayPushPop(n: number): number {
  const arr: number[] = [];
  for (let i = 0; i < n; i++) {
    arr.push(i);
  }
  let sum = 0;
  while (arr.length > 0) {
    sum += arr.pop()!;
  }
  return sum;
}

function benchArrayLength(arr: number[], n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum += arr.length;
  }
  return sum;
}

function benchArrayIterate(arr: number[]): number {
  let sum = 0;
  for (let i = 0; i < arr.length; i++) {
    sum += arr[i];
  }
  return sum;
}

// Pre-allocate dense arrays
const arr1k = new Array(1000);
for (let i = 0; i < 1000; i++) arr1k[i] = i;

const arr10k = new Array(10000);
for (let i = 0; i < 10000; i++) arr10k[i] = i;

const N = 1_000_000;
const ITERS = 50;

const start = Date.now();
for (let iter = 0; iter < ITERS; iter++) {
  benchArrayRead(arr1k, N);
  benchArrayWrite(arr1k, N);
  benchArrayPushPop(10_000);
  benchArrayLength(arr1k, N);
  benchArrayIterate(arr10k);
}
const elapsed = Date.now() - start;
console.log(`dense_array: ${elapsed}ms (${ITERS} iterations, N=${N})`);
