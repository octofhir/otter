// Inlined bodies with loops: compare the optimizing tier against the interpreter and node.
function assq(key, list) {
  while (list !== null) {
    if (list.car.car === key) return list.car;
    list = list.cdr;
  }
  return false;
}
function Pair(car, cdr) { this.car = car; this.cdr = cdr; }
function sumTo(n) { let s = 0; for (let i = 0; i < n; i++) { if (i % 3 === 0) continue; s += i; } return s; }
function nested(n) {
  let t = 0;
  outer: for (let i = 0; i < n; i++) {
    for (let j = 0; j < n; j++) {
      if (j > i) continue outer;
      if (i * j > 40) break outer;
      t += i ^ j;
    }
  }
  return t;
}
function findIndex(arr, x) { for (let i = 0; i < arr.length; i++) if (arr[i] === x) return i; return -1; }
function throwsInLoop(n) { for (let i = 0; i < n; i++) { if (i === 7) throw new RangeError("at " + i); } return n; }
function mixed(n, v) { let acc = v; for (let i = 0; i < n; i++) acc = acc + v; return acc; }
let counter = 0;
function bump(n) { for (let i = 0; i < n; i++) counter++; return counter; }

let list = null;
for (let i = 0; i < 20; i++) list = new Pair(new Pair("k" + i, i), list);
const arr = [];
for (let i = 0; i < 50; i++) arr.push(i * 7 % 13);

let checksum = 0;
for (let round = 0; round < 30000; round++) {
  const hit = assq("k" + (round % 25), list);
  checksum = (checksum + (hit === false ? -1 : hit.cdr) + sumTo(round % 17) + nested(round % 9)) | 0;
  checksum = (checksum + findIndex(arr, round % 14)) | 0;
  try { throwsInLoop(round % 10); } catch (e) { checksum = (checksum + e.message.length) | 0; }
  checksum = (checksum + (bump(round % 4) & 255)) | 0;
  // Type change late: the inlined loop deopts on a string accumulator.
  if (round > 25000) checksum = (checksum + String(mixed(3, round % 2 ? "x" : 1)).length) | 0;
  else checksum = (checksum + mixed(3, round % 5)) | 0;
}
console.log(checksum, counter);
