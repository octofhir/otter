// A WeakMap value reachable only through a live key survives full collections
// triggered by old-space growth, not only explicit debug collections.
const wm = new WeakMap();
const keys = [];
for (let i = 0; i < 400; i++) {
  const k = { i };
  keys.push(k);
  wm.set(k, { payload: "v" + i, arr: [i, i + 1, i + 2] });
}
const keep = [];
for (let round = 0; round < 16; round++) {
  const chunk = [];
  for (let j = 0; j < 300; j++) chunk.push(new Array(1000).fill(j));
  keep.push(chunk);
  if (keep.length > 6) keep.shift();
}
let bad = 0;
for (let i = 0; i < keys.length; i++) {
  const v = wm.get(keys[i]);
  if (!v || v.payload !== "v" + i || v.arr[1] !== i + 1) bad++;
}
JSON.stringify({ bad, kept: keep.length });
