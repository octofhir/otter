// Generator JIT bail-on-yield benchmark
// Tests that generators with hot loops can be JIT-compiled,
// bailing out at yield points and resuming in the interpreter.

function* range(n) {
  for (let i = 0; i < n; i++) {
    yield i;
  }
}

let sum = 0;
for (const v of range(10000)) {
  sum += v;
}
console.log("generator sum=" + sum); // 49995000

function* fib() {
  let a = 0, b = 1;
  while (true) {
    yield a;
    [a, b] = [b, a + b];
  }
}

const g = fib();
let fibSum = 0;
for (let i = 0; i < 10000; i++) {
  fibSum += g.next().value;
}
console.log("fib sum=" + fibSum);
