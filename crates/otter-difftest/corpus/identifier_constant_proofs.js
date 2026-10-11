// Immutable value folding retains lexical, eval and with environment reads.
let checks = 0;
function check(value, label) {
  if (!value) throw new Error(label);
  checks++;
}
globalThis.identifierOrdinary = 7;
function ordinary(local) { return local + identifierOrdinary; }
function canonical() { return [NaN, Infinity]; }
function parameters(NaN, Infinity) { return [NaN, Infinity]; }
function captured() {
  let NaN = 11, Infinity = 13;
  return {read() { return [NaN, Infinity]; }, write(value) { NaN = value; Infinity = value + 2; }};
}
const cells = captured();
for (let i = 0; i < 6000; i++) {
  ordinary(i); canonical(); parameters(i, i + 1); cells.read();
}
check(ordinary(3) === 10, "ordinary read");
identifierOrdinary = 17;
check(ordinary(3) === 20, "ordinary global replacement");
const constants = canonical();
check(Number.isNaN(constants[0]) && constants[1] === Infinity, "canonical values");
check(parameters(19, 23).join() === "19,23", "parameter shadows");
check(cells.read().join() === "11,13", "captured shadows");
cells.write(29);
check(cells.read().join() === "29,31", "captured mutation");
{
  const NaN = 37, Infinity = 41;
  check([NaN, Infinity].join() === "37,41", "block shadows");
}
function early() { return NaN; let NaN = 1; }
function cross(flag) {
  switch (flag) {
    case 0: return Infinity;
    case 1: let Infinity = 43; return Infinity;
  }
}
for (const invoke of [early, () => cross(0)]) {
  let name = "";
  try { invoke(); } catch (error) { name = error.name; }
  check(name === "ReferenceError", "lexical tdz");
}
check(cross(1) === 43, "initialized cross-clause shadow");
function evalShadow() {
  eval("var NaN = 47; var Infinity = 53;");
  return [NaN, Infinity];
}
check(evalShadow().join() === "47,53", "eval extension shadows");
function dynamic(env) { with (env) { return [NaN, Infinity]; } }
let reads = 0, allocations = 0;
const scope = {
  get NaN() { reads++; return 59; },
  get Infinity() {
    reads++;
    const values = [];
    for (let i = 0; i < 128; i++) { values.push({i}); allocations++; }
    return values[127].i;
  }
};
check(dynamic(scope).join() === "59,127" && reads === 2 && allocations === 128,
  "with getter values and effects");
const blocked = function () { throw new Error("unscopables is a property container"); };
blocked.NaN = true;
blocked.Infinity = true;
scope[Symbol.unscopables] = blocked;
const fallback = dynamic(scope);
check(Number.isNaN(fallback[0]) && fallback[1] === Infinity && reads === 2,
  "callable unscopables blocks both names");
delete scope[Symbol.unscopables];
function nested(outer, inner) {
  with (outer) { with (inner) { return [NaN, Infinity]; } }
}
check(nested({NaN: 61, Infinity: 67}, {}).join() === "61,67", "nested with miss");
const nestedFallback = nested({}, {});
check(Number.isNaN(nestedFallback[0]) && nestedFallback[1] === Infinity,
  "nested with global constants");
const retained = [];
for (let i = 0; i < 512; i++) retained.push({i, values: [i, i + 1]});
check(retained[511].values[1] === 512 && cells.read().join() === "29,31",
  "captured values survive allocation pressure");
console.log("identifier-constant-proofs", checks, reads, allocations);
