// The per-site PIC becomes terminal while the shared table remains usable.
const prototype = { value: 66, marker: 6 };
const inherited = Object.create(prototype);
inherited.inheritedMarker = 1;
const receivers = [
  { value: 11, a: 1 },
  { b: 2, value: 22 },
  { c: 3, d: 4, value: 33 },
  { value: 44, e: 5, f: 6 },
  { g: 7, value: 55, h: 8 },
  inherited,
];
function read(receiver) {
  for (let index = 0; index < 3; index++) {}
  return receiver.value;
}
const args = [receivers[0]];
for (let warm = 0; warm < 4010; warm++) {
  args[0] = receivers[warm % receivers.length];
  Reflect.apply(read, undefined, args);
}
const results = [];
for (let index = 0; index < receivers.length; index++) {
  args[0] = receivers[index];
  results.push(Reflect.apply(read, undefined, args));
}
const fresh = { novel: 1, value: 73, tail: 2 };
args[0] = fresh;
results.push(Reflect.apply(read, undefined, args));
fresh[Symbol("metadata")] = 1;
fresh.value = "current";
results.push(Reflect.apply(read, undefined, args));
const replacement = { value: 77, marker: 7 };
Object.setPrototypeOf(inherited, replacement);
args[0] = inherited;
results.push(Reflect.apply(read, undefined, args));
let effects = 0;
const sentinel = {};
Object.defineProperty(replacement, "value", { get() {
  effects++;
  const retained = [];
  for (let index = 0; index < 128; index++) retained.push({ index });
  if (retained[127].index !== 127) throw new Error("moving roots lost");
  return sentinel;
} });
results.push(Reflect.apply(read, undefined, args) === sentinel, effects);
args[0] = new Proxy({ value: 92 }, { get(target, key) { effects++; return target[key]; } });
results.push(Reflect.apply(read, undefined, args), effects);
const result = JSON.stringify(results);
console.log(result);
result;
