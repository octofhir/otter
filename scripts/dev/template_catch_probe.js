"use strict";
function thrower(x) { if (x % 1000 === 999) throw new Error("boom" + x); return x; }
function hot(n) {
  let acc = 0;
  for (let i = 0; i < n; i++) {
    try { acc += thrower(i); } catch (e) { acc += e.message.length; }
  }
  return acc;
}
let total = 0;
for (let round = 0; round < 20; round++) total += hot(20000);
console.log(total);
