// Micro-benchmarks for JIT operation throughput
"use strict";

const N = 500000;

function benchArith(n) {
  let x = 0;
  for (let i = 0; i < n; i++) {
    x = (x + i * 3) | 0;
  }
  return x;
}

function benchPropRead(n) {
  let obj = { x: 42 };
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += obj.x;
  }
  return s;
}

function benchArrayLength(n) {
  let arr = [1, 2, 3, 4, 5];
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += arr.length;
  }
  return s;
}

function benchArrayAccess(n) {
  let arr = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += arr[i % 10];
  }
  return s;
}

function benchCall(n) {
  function add1(x) { return x + 1; }
  let s = 0;
  for (let i = 0; i < n; i++) {
    s = s + add1(i);
  }
  return s;
}

console.log("arith:", benchArith(N));
console.log("propRead:", benchPropRead(N));
console.log("arrayLength:", benchArrayLength(N));
console.log("arrayAccess:", benchArrayAccess(N));
console.log("call:", benchCall(N));
