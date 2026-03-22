// JIT benchmark: time-to-first-native-execution
// Measures: warmup latency, how quickly interpreter hands off to JIT

// Many small functions that should each become hot quickly
function f1(x: number): number { return x + 1; }
function f2(x: number): number { return x * 2; }
function f3(x: number): number { return x - 1; }
function f4(x: number): number { return x / 2; }
function f5(x: number): number { return x % 7; }

function chainSmall(x: number): number {
  return f5(f4(f3(f2(f1(x)))));
}

// Measure how many iterations until functions are "warm"
function measureWarmup(fn: (n: number) => number, label: string): void {
  const BATCH = 100;
  const MAX_BATCHES = 100;
  const times: number[] = [];

  for (let batch = 0; batch < MAX_BATCHES; batch++) {
    const start = Date.now();
    for (let i = 0; i < BATCH; i++) {
      fn(i + batch * BATCH);
    }
    const elapsed = Date.now() - start;
    times.push(elapsed);
  }

  // Find when execution stabilized (first batch where time <= 1.2x final batch)
  const finalTime = times[times.length - 1] || 1;
  let stableAt = times.length;
  for (let i = 0; i < times.length; i++) {
    if (times[i] <= finalTime * 1.2 + 1) {
      stableAt = i;
      break;
    }
  }

  console.log(
    `${label}: stable at batch ${stableAt}/${MAX_BATCHES}, ` +
    `first=${times[0]}ms, last=${times[times.length - 1]}ms`
  );
}

// A function that does real work to make timing meaningful
function workload(n: number): number {
  let sum = 0;
  for (let i = 0; i < 10000; i++) {
    sum += chainSmall(i + n);
  }
  return sum;
}

const start = Date.now();
measureWarmup(workload, "chainSmall");
measureWarmup(
  (n: number) => {
    const arr = [1, 2, 3, 4, 5];
    let sum = 0;
    for (let i = 0; i < 10000; i++) {
      sum += arr[i % arr.length] + n;
    }
    return sum;
  },
  "array_access"
);
measureWarmup(
  (n: number) => {
    const obj = { a: 1, b: 2, c: 3 };
    let sum = 0;
    for (let i = 0; i < 10000; i++) {
      sum += obj.a + obj.b + obj.c + n;
    }
    return sum;
  },
  "prop_access"
);
const elapsed = Date.now() - start;
console.log(`warmup: ${elapsed}ms total`);
