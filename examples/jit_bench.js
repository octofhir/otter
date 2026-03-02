// JIT benchmark: hot loops, arithmetic, property access, function calls
"use strict";

function fib(n) {
  if (n < 2) return n;
  let a = 0, b = 1;
  for (let i = 2; i <= n; i++) {
    let t = a + b;
    a = b;
    b = t;
  }
  return b;
}

function sumArray(arr) {
  let s = 0;
  for (let i = 0; i < arr.length; i++) {
    s += arr[i];
  }
  return s;
}

function propertyChain(n) {
  let obj = { x: 1 };
  let total = 0;
  for (let i = 0; i < n; i++) {
    total += obj.x;
    obj.x = total;
  }
  return total;
}

function nestedCalls(n) {
  function inner(x) { return x * 2 + 1; }
  let acc = 0;
  for (let i = 0; i < n; i++) {
    acc += inner(i);
  }
  return acc;
}

function comparison(n) {
  let count = 0;
  for (let i = 0; i < n; i++) {
    if (i % 3 === 0) count++;
    else if (i % 5 === 0) count += 2;
    else count += 3;
  }
  return count;
}

function pureArith(n) {
  let x = 0;
  for (let i = 0; i < n; i++) {
    x = (x + i * 3) ^ (i >> 1);
  }
  return x;
}

// Run each benchmark
const N = 100000;

let r1 = fib(40);
console.log("fib(40):", r1);

let arr = [];
for (let i = 0; i < N; i++) arr.push(i);
let r2 = sumArray(arr);
console.log("sumArray:", r2);

let r3 = propertyChain(N);
console.log("propertyChain:", r3);

let r4 = nestedCalls(N);
console.log("nestedCalls:", r4);

let r5 = comparison(N);
console.log("comparison:", r5);

let r6 = pureArith(N);
console.log("pureArith:", r6);
