// Keyed reads of objects already in dictionary mode: forty computed keys
// push each past the keyed fast-property limit.
const names = [];
for (let i = 0; i < 400; i++) names.push('ident' + i);
const maps = [];
for (let m = 0; m < 2000; m++) {
  const map = Object.create(null);
  for (let k = 0; k < 40; k++) map[names[(m * 7 + k * 13) % 400]] = k;
  maps.push(map);
}
function get(map, key) { const v = map[key]; return v !== undefined ? v : null; }
let hits = 0;
const t = performance.now();
for (let r = 0; r < 300; r++) for (let m = 0; m < 2000; m++) { if (get(maps[m], names[(m * 7 + r) % 400]) !== null) hits++; }
console.log(hits, (performance.now() - t).toFixed(1), 'ms');
