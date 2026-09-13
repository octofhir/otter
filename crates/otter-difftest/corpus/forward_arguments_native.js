// Live arguments, capture allocation, target growth and committed abrupt exits.
function makeFirst(tag) {
  return function first(a, b) {
    function read() { return a.value; }
    return tag + arguments.length + read() + b + arguments[2].value;
  };
}
function second(a, b) {
  'use strict';
  return 20 + arguments.length + a.value + b + arguments[2].value;
}
let selected = makeFirst(10);
function forward(a, b, c) {
  a = { value: (a === undefined ? 0 : a.value) + 1 };
  return selected.apply(null, arguments);
}
let sum = 0;
for (let i = 0; i < 2048; i++) sum += forward({ value: i }, 2, { value: 3 });
selected = second;
for (let i = 0; i < 2048; i++) sum += forward({ value: i }, 2, { value: 3 });
function count() { return arguments.length; }
selected = count;
for (let i = 0; i < 1024; i++) {
  if (forward({ value: i }, 2, { value: 3 }) !== 3) throw new Error('lost warm actuals');
}
const many = [{ value: 1 }, 2, { value: 3 }];
for (let i = 3; i < 600; i++) many.push(i);
const counts = [forward.apply(null, many), forward({ value: 1 }), forward()];
const token = { marker: 91 };
function throwing() {
  if (arguments.length !== 3) throw new Error('lost throwing actuals');
  throw token;
}
selected = throwing;
let nativeThrows = 0;
for (let i = 0; i < 1024; i++) {
  try { forward({ value: i }, 2, { value: 3 }); }
  catch (error) { if (error !== token) throw new Error('lost callee throw'); nativeThrows++; }
}
let customCalls = 0;
throwing.apply = function() { customCalls++; throw token; };
let coldThrows = 0;
for (let i = 0; i < 1024; i++) {
  try { forward({ value: i }, 2, { value: 3 }); }
  catch (error) { if (error !== token) throw new Error('lost apply throw'); coldThrows++; }
}
if (customCalls !== coldThrows) throw new Error('replayed custom apply');
JSON.stringify([sum, counts, nativeThrows, coldThrows]);
