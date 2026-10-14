// Shape-owned integer fields keep their representation only while every
// writer preserves it. Widening and layout changes occur after hot reads.
const results = [];
function Inline(value) { this.value = value; }
Inline.prototype.read = function () { return this.value; };
Inline.prototype.bump = function () { return ++this.value; };
function Suffix(value) {
  this.a = 1; this.b = 2; this.c = 3; this.d = 4;
  this.e = 5; this.f = 6; this.g = 7; this.h = 8;
  this.value = value;
}
Suffix.prototype.read = Inline.prototype.read;
Suffix.prototype.bump = Inline.prototype.bump;
function sum(cell, rounds) {
  let total = 0;
  for (let i = 0; i < rounds; i++) total += cell.read();
  return total;
}
function bump(cell, rounds) {
  let total = 0;
  for (let i = 0; i < rounds; i++) total += cell.bump();
  return total;
}
for (const Make of [Inline, Suffix]) {
  const first = new Make(-7);
  results.push(sum(first, 10000), bump(first, 10000));
  first.value = 1.25;
  results.push(sum(first, 10000), bump(first, 10000));
  first.value = null;
  results.push(sum(first, 10000), first.read());
  first.value = undefined;
  results.push(Number.isNaN(sum(first, 10000)), first.read());
  first.value = { valueOf() { return 3; } };
  results.push(sum(first, 10000), typeof first.read());
  first.value = 'x';
  results.push(sum(first, 32));
}

// An independent field lineage supplies a warm integer-only owner.
function Overflow() { this.count = 1; }
Overflow.prototype.next = function () { return ++this.count; };
const overflow = new Overflow();
let overflowTotal = 0;
for (let i = 0; i < 10000; i++) overflowTotal += overflow.next();
overflow.count = 2147483647;
results.push(overflowTotal, overflow.next(), overflow.next());

// A native store loop widens an integer field before its next read.
function NativeStore() { this.slot = 2; }
NativeStore.prototype.read = function () { return this.slot; };
const native = new NativeStore();
function storeAndRead(cell) {
  let total = 0;
  for (let i = 0; i < 24000; i++) {
    cell.slot = i < 16000 ? (i & 31) : i + 0.5;
    total += cell.read();
  }
  return total;
}
results.push(storeAndRead(native), native.read());

// Reflect/descriptor writes and a later accessor must retire the load proof.
function Descriptor() { this.field = 4; }
Descriptor.prototype.read = function () { return this.field; };
const descriptor = new Descriptor();
function readDescriptor(cell, rounds) {
  let total = 0;
  for (let i = 0; i < rounds; i++) total += cell.read();
  return total;
}
results.push(readDescriptor(descriptor, 10000));
Reflect.set(descriptor, 'field', -1.75);
results.push(readDescriptor(descriptor, 10000));
Object.defineProperty(descriptor, 'field', { value: 6, writable: true });
results.push(readDescriptor(descriptor, 10000));
let gets = 0;
Object.defineProperty(descriptor, 'field', { configurable: true, get() { gets++; return 9; } });
results.push(readDescriptor(descriptor, 100000), gets);

// Mixed categories in one defining owner never grant an ordinary-object load.
function Mixed(value) { this.link = value; }
const mixedNumber = new Mixed(5);
const mixedObject = new Mixed({ amount: 7 });
function mixedRead(cell) { return typeof cell.link === 'object' ? cell.link.amount : cell.link; }
let mixedTotal = 0;
for (let i = 0; i < 24000; i++) mixedTotal += mixedRead(i & 1 ? mixedObject : mixedNumber);
results.push(mixedTotal);

// Array expando storage also widens after hot reads.
const array = [];
array.count = 1;
function arrayRead(value) { return value.count; }
let arrayTotal = 0;
for (let i = 0; i < 12000; i++) arrayTotal += arrayRead(array);
array.count = 2.5;
for (let i = 0; i < 12000; i++) arrayTotal += arrayRead(array);
results.push(arrayTotal);
console.log(JSON.stringify(results));
