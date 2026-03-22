// JIT benchmark: upvalue access in hot loops
// Measures: closure capture efficiency, upvalue cell read/write, shared captures

function benchClosureRead(n: number): number {
  let captured = 42;
  const read = () => captured;
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum += read();
  }
  return sum;
}

function benchClosureWrite(n: number): number {
  let captured = 0;
  const inc = () => { captured++; };
  for (let i = 0; i < n; i++) {
    inc();
  }
  return captured;
}

function benchClosureReadWrite(n: number): number {
  let a = 0;
  let b = 1;
  const step = () => {
    const tmp = a + b;
    a = b;
    b = tmp;
    return a;
  };
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum += step();
    if (b > 1_000_000) { a = 0; b = 1; }
  }
  return sum;
}

function benchSharedCapture(n: number): number {
  let shared = 0;
  const inc = () => { shared++; };
  const get = () => shared;

  for (let i = 0; i < n; i++) {
    inc();
  }
  return get();
}

function benchNestedCapture(n: number): number {
  let outer = 0;
  const makeInner = () => {
    let inner = 0;
    return () => {
      inner++;
      outer += inner;
      return outer;
    };
  };
  const fn1 = makeInner();
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum += fn1();
  }
  return sum;
}

const N = 1_000_000;
const ITERS = 50;

const start = Date.now();
for (let iter = 0; iter < ITERS; iter++) {
  benchClosureRead(N);
  benchClosureWrite(N);
  benchClosureReadWrite(N);
  benchSharedCapture(N);
  benchNestedCapture(N);
}
const elapsed = Date.now() - start;
console.log(`closure_capture: ${elapsed}ms (${ITERS} iterations, N=${N})`);
