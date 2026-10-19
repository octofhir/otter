// Concatenation sites that tenure (retained), decline (dropped) and mix
// number text, cons ropes and short flat results, checked by content.
function leaf(tag) { return 'String for key ' + tag + ' in leaf node'; }
function key(i) { return 'k' + i; }
const kept = [];
let dropped = 0;
for (let round = 0; round < 40; round++) {
  for (let i = 0; i < 4000; i++) {
    const tag = Math.sin(round * 4000 + i);
    kept.push(leaf(tag));
    dropped += key(i).length + `${tag}-${i}`.length;
  }
  if (kept.length > 60000) kept.splice(0, 20000);
}
let check = 0;
for (let i = 0; i < kept.length; i += 997) {
  const s = kept[i];
  if (!s.startsWith('String for key ') || !s.endsWith(' in leaf node')) throw new Error('bad ' + s);
  check += s.length;
}
console.log(kept.length, check, dropped);
