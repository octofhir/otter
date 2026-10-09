// for-in over dictionary-mode objects: deletions before and during the
// loop, integer-like keys, additions, non-enumerable keys, and prototypes
// that do or do not contribute keys.
function keysOf(o) { const out = []; for (const k in o) out.push(k); return out.join(","); }
const out = [];
for (let round = 0; round < 200; round++) {
  const d = {};
  for (let i = 0; i < 40; i++) d["k" + ((i * 7) % 40)] = i;
  delete d.k5; delete d.k12;
  d[10] = "ten"; d[2] = "two"; d["01"] = "lead";
  Object.defineProperty(d, "hidden", { value: 1, enumerable: false });
  let seen = 0;
  for (const k in d) { if (k === "k7") delete d.k14; if (k === "k0") d.added = 1; seen++; }
  if (round % 50 === 0) out.push(keysOf(d), seen);
}
const protoWithKeys = { inherited: 1, k1: "shadowed" };
const child = Object.create(protoWithKeys);
for (let i = 0; i < 40; i++) child["c" + i] = i;
delete child.c3;
out.push(keysOf(child));
const nullProto = Object.create(null);
for (let i = 0; i < 30; i++) nullProto["n" + i] = i;
delete nullProto.n0;
out.push(keysOf(nullProto));
Object.prototype.leak = 1;
const leaky = {}; for (let i = 0; i < 30; i++) leaky["l" + i] = i; delete leaky.l1;
out.push(keysOf(leaky));
delete Object.prototype.leak;
out.push(keysOf(leaky));
console.log(out.join("\n"));
