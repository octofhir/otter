"use strict";
let sum = 0;
const obj = { x: 1, y: 2 };
function props() { let s = 0; for (let i = 0; i < 1000; i++) s += obj.x + obj.y; sum += s; }
for (let i = 0; i < 20000; i++) props();
console.log(sum);
