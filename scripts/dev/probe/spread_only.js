"use strict";
let sum = 0;
function f(a, b) { sum += a + b; }
const pair = [1, 2];
function run() { for (let i = 0; i < 1000; i++) f(...pair); }
for (let i = 0; i < 20000; i++) run();
console.log(sum);
