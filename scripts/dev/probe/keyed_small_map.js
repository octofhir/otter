// `this.map[key]` over many small shaped maps, interned keys, mostly absent.
function StringMap() { this.map = {}; }
StringMap.prototype.get = function (key) { var v = this.map[key]; return v !== undefined ? v : null; };
const names = [];
for (let i = 0; i < 3000; i++) names.push("name" + i);
for (const n of names) ({})[n] = 1; // intern every key
const maps = [];
for (let j = 0; j < 200; j++) {
  const m = new StringMap();
  for (let k = 0; k < 4; k++) m.map[names[(j * 13 + k * 101) % 3000]] = k;
  maps.push(m);
}
let hits = 0;
const t = Date.now();
for (let r = 0; r < 100; r++) {
  for (let i = 0; i < 6000; i++) {
    if (maps[i % 200].get(names[(i * 7) % 3000]) !== null) hits++;
  }
}
console.log(hits, Date.now() - t, "ms");
