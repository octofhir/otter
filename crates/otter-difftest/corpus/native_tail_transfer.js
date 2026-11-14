"use strict";
// Own argument identities retain the physical activations exercised by the
// runtime's exact-generation proofs. Every tail shape uses the same payload.
function child(payload, value, missing) {
  if (arguments === payload) throw "child arguments identity";
  payload.calls++;
  const kept = { payload, value };
  const pressure = [payload, kept, missing];
  if (pressure[0] !== kept.payload || pressure[1] !== kept) throw "tail root";
  if (value < 0) throw kept;
  return [value + 1, arguments.length, missing, kept.payload];
}
function fit(payload, value) {
  if (arguments === payload) throw "tail arguments identity";
  return child(payload, value);
}
function grow(payload, value) {
  if (arguments === payload) throw "tail arguments identity";
  return child(payload, value, undefined, 7, 8);
}
function zero() {
  if (arguments === undefined) throw "zero arguments identity";
  return noActuals();
}
function noActuals() {
  if (arguments === undefined) throw "callee arguments identity";
  return arguments.length;
}
function bridge(fn, payload, value) {
  const result = fn(payload, value);
  return result;
}
function Constructor(payload, value) {
  if (arguments === payload) throw "constructor arguments identity";
  this.payload = payload;
  return fit(payload, value);
}
let checksum = 0;
for (let i = 0; i < 768; i++) {
  const payload = { calls: 0, marker: i };
  const first = bridge(fit, payload, i);
  const second = bridge(grow, payload, i);
  if (first[3] !== payload || second[3] !== payload || payload.calls !== 2
      || first[1] !== 2 || second[1] !== 5 || first[2] !== undefined
      || second[2] !== undefined || zero() !== 0) throw "tail actuals";
  checksum += first[0] + second[0];
}
const payload = { calls: 0, marker: 91 };
const wide = bridge(grow, payload, 2147483647);
let thrown;
try { bridge(grow, payload, -1); } catch (error) {
  thrown = error.payload === payload && error.value === -1;
}
const constructed = new Constructor(payload, 7);
if (checksum !== 590592 || wide[0] !== 2147483648 || !thrown
    || constructed[0] !== 8 || payload.calls !== 3) throw "tail completion";
console.log(JSON.stringify([checksum, wide.slice(0, 3), thrown,
  constructed.slice(0, 3), payload.calls, constructed[3] === payload]));
