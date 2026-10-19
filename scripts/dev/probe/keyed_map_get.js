// `this.map[key]` over a dictionary map with computed keys, half absent.
function StringMap() { this.map = Object.create(null); }
StringMap.prototype.get = function (key) { var v = this.map[key]; return v !== undefined ? v : null; };
StringMap.prototype.set = function (key, v) { this.map[key] = v; };
const m = new StringMap();
for (let i = 0; i < 2000; i++) m.set("k" + i, i);
let hits = 0;
const t = Date.now();
for (let r = 0; r < 300; r++) {
  for (let i = 0; i < 4000; i++) {
    if (m.get("k" + (i * 7 % 4000)) !== null) hits++;
  }
}
console.log(hits, Date.now() - t, "ms");
