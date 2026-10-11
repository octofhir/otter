// Literal allocation sites whose cells survive are allocated old; their young
// children, later stores and element arrays stay visible across collections.
const results = [];
function leaf(key) {
  return { array: [key, key + 1, key + 2, key + 3], label: 'leaf ' + key };
}
function tree(depth, key) {
  if (depth === 0) return leaf(key);
  return { left: tree(depth - 1, key * 2), right: tree(depth - 1, key * 2 + 1) };
}
function sum(node) {
  if (node.array !== undefined) return node.array[3] + node.label.length;
  return sum(node.left) + sum(node.right);
}
// Enough retained literals to fill and grow the nursery several times.
const kept = [];
let total = 0;
for (let round = 0; round < 1200; round++) {
  const t = tree(5, round);
  kept.push(t);
  total += sum(t);
  if (kept.length > 600) kept.shift();
}
results.push(total, kept.length, sum(kept[0]), sum(kept[kept.length - 1]));

// Young values stored into cells born old after the site decided.
let mutated = 0;
for (let i = 0; i < kept.length; i++) {
  const t = kept[i];
  t.left.extra = { value: i, tag: 'young ' + i };
  t.right = { array: [i, i, i, i], label: 'swap' };
}
for (let round = 0; round < 400; round++) {
  const garbage = tree(4, round);
  mutated += garbage.left.left.left.left.array[0] & 1;
}
for (let i = 0; i < kept.length; i++) {
  const t = kept[i];
  mutated += t.left.extra.value + t.left.extra.tag.length + sum(t.right);
}
results.push(mutated);

// Tagged and numeric arrays from one retained site, with object elements.
function pair(a, b) { return [a, b, { a, b }]; }
const pairs = [];
for (let i = 0; i < 40000; i++) pairs.push(pair(i, 'p' + i));
let paired = 0;
for (let i = 0; i < pairs.length; i += 997) paired += pairs[i][0] + pairs[i][1].length + pairs[i][2].a;
results.push(paired, pairs[pairs.length - 1][2].b);

// A short-lived site next to a retained one keeps both correct.
function temp(i) { return { i, s: 'tmp' + i }; }
let temps = 0;
for (let i = 0; i < 200000; i++) temps += temp(i).i & 3;
results.push(temps);

// Holes and doubles in a retained array literal.
function mixed(i) { return [i + 0.5, , i]; }
const mixes = [];
for (let i = 0; i < 30000; i++) mixes.push(mixed(i));
let holes = 0;
for (const m of mixes) holes += (1 in m ? 1 : 0) + m[0] + m[2];
results.push(holes, mixes.length);

console.log(JSON.stringify(results));
