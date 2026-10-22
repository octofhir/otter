// Shared Tagged literals detach before native append, including lengths larger
// than the minimum growth capacity. Both shells survive collection; the appended
// object must keep its identity and the sibling's entire prefix stays unchanged.
"use strict";

function tagged4() { return [null, true, false, null]; }
function tagged8() { return [null, true, false, null, true, false, null, true]; }
function tagged17() {
  return [null, true, false, null, true, false, null, true, false,
    null, true, false, null, true, false, null, true];
}
const left4 = tagged4(), right4 = tagged4();
const left8 = tagged8(), right8 = tagged8();
const left17 = tagged17(), right17 = tagged17();

function append(array, child) {
  if (child === null) return append(array, {marker: 0});
  const length = array.push(child);
  return array[length - 1];
}
const warmArray = [null, true, false, null, true, false, null, true];
warmArray.pop();
const warmChild = {marker: 731};
for (let warm = 0; warm < 20000; warm++) {
  append(warmArray, warmChild);
  warmArray.pop();
}
if (typeof gc === "function") gc();

function check(left, right, size) {
  const child = {marker: 900 + size};
  const result = append(left, child);
  if (typeof gc === "function") gc();
  if (result !== child || left[size] !== child || result.marker !== 900 + size)
    throw new Error("shared append identity " + size);
  if (left.length !== size + 1 || right.length !== size || right[size] !== undefined)
    throw new Error("shared append length " + size);
  for (let i = 0; i < size; i++) {
    const expected = i % 3 === 0 ? null : i % 3 === 1;
    if (left[i] !== expected || right[i] !== expected)
      throw new Error("shared append prefix " + size + ":" + i);
  }
  return result.marker;
}
console.log("shared-tagged-growth", check(left4, right4, 4),
  check(left8, right8, 8), check(left17, right17, 17));
