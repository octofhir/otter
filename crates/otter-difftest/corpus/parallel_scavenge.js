// Young collections with megabytes of survivors, traced by several threads:
// deep and wide object graphs, arrays of mixed elements, strings, Maps and
// WeakMaps whose keys die or survive, closures, and old objects that keep
// gaining young children between collections.
const results = [];

function Node(key, left, right) {
  this.key = key;
  this.left = left;
  this.right = right;
  this.payload = { text: 'k' + key, list: [key, key * 2, key * 3] };
}

function build(depth, base) {
  if (depth === 0) return null;
  return new Node(base, build(depth - 1, base * 2), build(depth - 1, base * 2 + 1));
}

function checksum(node) {
  let sum = 0;
  const stack = [node];
  while (stack.length) {
    const n = stack.pop();
    if (!n) continue;
    sum = (sum * 31 + n.key + n.payload.list[2] + n.payload.text.length) % 1000000007;
    stack.push(n.left, n.right);
  }
  return sum;
}

// Old roots that keep receiving young children.
const keep = [];
for (let i = 0; i < 64; i++) keep.push({ id: i, child: null, items: [] });

const weak = new WeakMap();
const strong = new Map();
let rounds = 0;
for (let round = 0; round < 16; round++) {
  const tree = build(10, round + 1);
  const holder = keep[round % keep.length];
  holder.child = tree;
  holder.items.push({ round, tree: round % 3 === 0 ? tree : null, s: 'r' + round });
  if (holder.items.length > 6) holder.items.shift();
  const key = { round };
  weak.set(key, { tree, round });
  if (round % 2 === 0) strong.set(round, key);
  const garbage = [];
  for (let i = 0; i < 6000; i++) garbage.push({ i, s: 'g' + (i & 255) });
  const closures = [];
  for (let i = 0; i < 500; i++) closures.push(() => i + round);
  rounds = (rounds + checksum(tree) + garbage.length + closures[499]()) % 1000000007;
}
results.push(rounds);

let held = 0;
for (const holder of keep) {
  held = (held + (holder.child ? checksum(holder.child) : 0)) % 1000000007;
  for (const item of holder.items) held = (held + item.round + item.s.length + (item.tree ? 1 : 0)) % 1000000007;
}
results.push(held);

let weakLive = 0;
for (const [round, key] of strong) {
  const entry = weak.get(key);
  weakLive = (weakLive + round + (entry ? checksum(entry.tree) % 1000 : 0)) % 1000000007;
}
results.push(weakLive, strong.size);

// Large mixed arrays that survive several collections while young values
// keep replacing their elements.
const mixed = [];
for (let i = 0; i < 30000; i++) mixed.push(i % 3 === 0 ? { v: i } : i % 3 === 1 ? 'x' + (i % 97) : i * 0.5);
let mixedSum = 0;
for (let round = 0; round < 8; round++) {
  for (let i = round; i < mixed.length; i += 37) mixed[i] = { v: i + round, next: mixed[(i * 7) % mixed.length] };
  const churn = [];
  for (let i = 0; i < 12000; i++) churn.push([i, i + 1]);
  for (let i = 0; i < mixed.length; i += 101) {
    const value = mixed[i];
    mixedSum = (mixedSum + (typeof value === 'object' ? value.v : typeof value === 'string' ? value.length : value)) % 1000000007;
  }
  mixedSum = (mixedSum + churn.length) % 1000000007;
}
results.push(mixedSum);

console.log(JSON.stringify(results));
