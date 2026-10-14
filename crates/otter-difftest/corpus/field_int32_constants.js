// Exact integer fields remain constants only while all instances and writers
// agree. Every change occurs after enough work to admit native readers.
const out = [];
const direction = { FORWARD: 1 };
function readDirection(record) { return record.FORWARD + arguments.length; }
let directions = 0;
for (let i = 0; i < 20000; i++) directions += readDirection(direction);
direction.FORWARD = 1;
for (let i = 0; i < 12000; i++) directions += readDirection(direction);
const sameDirection = { FORWARD: 1 };
out.push(directions, readDirection(sameDirection));
direction.FORWARD = 2;
for (let i = 0; i < 20000; i++) directions += readDirection(direction);
out.push(directions);

function Constant(value) { this.value = value; }
Constant.prototype.read = function readConstant() { return this.value; };
function total(cell, count) {
  let result = 0;
  for (let i = 0; i < count; i++) result += cell.read();
  return result;
}
const cell = new Constant(7);
out.push(total(cell, 12000));
cell.value = 7;
out.push(total(cell, 12000));
cell.value = 8;
out.push(total(cell, 12000));
cell.value = 7;
out.push(total(cell, 12000));
Reflect.set(cell, 'value', -7);
out.push(total(cell, 12000));
Object.defineProperty(cell, 'value', { value: 11 });
out.push(total(cell, 12000));
cell.value = 1.5;
out.push(total(cell, 12000));
cell.value = -0;
out.push(Object.is(cell.read(), -0));

// One source creates equal constants in distinct objects before a new value.
function Born(value) { this.fixed = value; }
Born.prototype.read = function readBorn() { return this.fixed; };
const first = new Born(-3);
let births = 0;
for (let i = 0; i < 16000; i++) births += new Born(-3).read();
const different = new Born(-4);
for (let i = 0; i < 12000; i++) births += (i & 1 ? first : different).read();
out.push(births, first.read(), different.read());

function bornLiteral(value) { return { BORN_CONSTANT: value }; }
function readLiteral(record) { return record.BORN_CONSTANT + arguments.length; }
let literals = 0;
for (let i = 0; i < 16000; i++) literals += readLiteral(bornLiteral(-3));
const changedLiteral = bornLiteral(-4);
for (let i = 0; i < 12000; i++) literals += readLiteral(changedLiteral);
out.push(literals);

// Native stores must observe a different integer, even with the same tag.
const writable = { NATIVE_CONSTANT: 5 };
function readWritable(record) { return record.NATIVE_CONSTANT + arguments.length; }
let writableWarmup = 0;
for (let i = 0; i < 16000; i++) writableWarmup += readWritable(writable);
out.push(writableWarmup);
function nativeWrite(record, changed) {
  let sum = 0;
  for (let i = 0; i < 24000; i++) {
    record.NATIVE_CONSTANT = i < 16000 ? 5 : changed;
    sum += record.NATIVE_CONSTANT;
  }
  return sum;
}
out.push(nativeWrite(writable, 6), readWritable(writable), nativeWrite(writable, 5));

// Nullish and shape unions erase the numeric payload before recording shapes.
const union = { UNION_CONSTANT: 7 };
function unionRead(record) {
  const value = record.UNION_CONSTANT;
  return value == null ? 'nullish' : typeof value === 'object' ? value.answer : value;
}
let warm = 0;
for (let i = 0; i < 12000; i++) warm += unionRead(union);
union.UNION_CONSTANT = null;
out.push(warm, unionRead(union));
union.UNION_CONSTANT = undefined;
out.push(unionRead(union));
union.UNION_CONSTANT = { answer: 9 };
let objects = 0;
for (let i = 0; i < 12000; i++) objects += unionRead(union);
out.push(objects);

const objectUnion = { OBJECT_CONSTANT: 7 };
function objectRead(record) {
  const value = record.OBJECT_CONSTANT;
  return (typeof value === 'object' ? value.answer : value) + arguments.length;
}
let objectWarmup = 0;
for (let i = 0; i < 16000; i++) objectWarmup += objectRead(objectUnion);
objectUnion.OBJECT_CONSTANT = { answer: 9 };
let objectResult = 0;
for (let i = 0; i < 16000; i++) objectResult += objectRead(objectUnion);
out.push(objectWarmup, objectResult);

// A suffix-bank constant changes through the ordinary keyed writer.
const suffix = {
  a: 0, b: 0, c: 0, d: 0, e: 0, f: 0, g: 0, h: 0,
  SUFFIX_CONSTANT: -2147483648,
  read() { return this.SUFFIX_CONSTANT; }
};
out.push(total(suffix, 12000));
const key = ['SUFFIX_', 'CONSTANT'].join('');
suffix[key] = 2147483647;
out.push(total(suffix, 12000));
console.log(JSON.stringify(out));
