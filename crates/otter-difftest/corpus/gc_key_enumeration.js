// Key and value enumeration over fresh objects while collections run between
// allocations: for-in, Object.keys/values/entries and Iterator.zipKeyed
// results must see every own enumerable property of the object they were
// given, whatever moved during the walk.
let checksum = 0;
for (let round = 0; round < 1500; round++) {
  const r = { value: round, done: round % 2 === 0, extra: "x" + round };
  let seen = 0;
  for (const k in r) seen += k.length;
  const values = Object.values(r);
  const entries = Object.entries(r);
  checksum = (checksum + seen + values.length + entries.length + Object.keys(r).length) | 0;
  if (values[0] !== round || entries[2][1] !== "x" + round) checksum = (checksum + 1000003) | 0;
}
const zipped = [];
for (let round = 0; round < 60; round++) {
  const it = Iterator.zipKeyed(
    { left: ["a", "b"], right: ["c"] },
    { mode: "longest", padding: { right: "pad" } },
  );
  for (const step of it) {
    const keys = [];
    for (const k in step) keys.push(k);
    zipped.push(keys.join("") + Object.values(step).join(""));
  }
}
console.log(checksum, zipped.length, zipped.slice(0, 2).join("|"));
