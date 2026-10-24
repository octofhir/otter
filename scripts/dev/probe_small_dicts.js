// Many small dictionaries made the way the TypeScript compiler makes them
// (create with null prototype, add and delete a key), filled with computed
// keys and read through one keyed site.
function createMap() { const map = Object.create(null); map.__ = undefined; delete map.__; return map; }
const names = [];
for (let i = 0; i < 400; i++) names.push('ident' + i);
const maps = [];
for (let m = 0; m < 2000; m++) {
  const map = createMap();
  const n = 1 + (m % 9);
  for (let k = 0; k < n; k++) map[names[(m * 7 + k * 13) % 400]] = k;
  maps.push(map);
}
function get(map, key) { const v = map[key]; return v !== undefined ? v : null; }
let hits = 0;
const t = performance.now();
for (let r = 0; r < 300; r++) for (let m = 0; m < 2000; m++) { if (get(maps[m], names[(m * 7 + r) % 400]) !== null) hits++; }
console.log(hits, (performance.now() - t).toFixed(1), 'ms');
