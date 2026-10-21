// Ternaries that only pick a value become selects in optimized code: picks
// of int32, float64 (NaN, -0, infinities) and tagged values on int32,
// float64, equality and boolean tests, fields of an object the test does not
// read, and a field read guarded by the test itself, which must stay below
// its edge, all answer exactly what the interpreter answers.
'use strict';
class Edge {
  constructor(v1, v2, direction) { this.v1 = v1; this.v2 = v2; this.direction = direction; }
  input() { return this.direction === 1 ? this.v1 : this.v2; }
  output() { return this.direction === 1 ? this.v2 : this.v1; }
}
function picks(n) {
  let ints = 0, floats = 0, tags = 0, nanSeen = 0, negZero = 0;
  const floatsIn = [1.5, -0, NaN, Infinity, -Infinity, 2.25, 0, -3.75];
  const names = ['a', 'b', null, undefined, 7, { k: 1 }];
  const edges = [new Edge({ value: 3 }, { value: 4 }, 1), new Edge({ value: 5 }, { value: 6 }, 0)];
  for (let i = 0; i < n; i++) {
    const a = i & 7, b = (i >> 3) & 7;
    ints = (ints + (a < b ? a : b) + (a === b ? 100 : -1) + (a >= 4 ? a * 3 : b - 1)) | 0;
    const x = floatsIn[a], y = floatsIn[b];
    const m = x < y ? x : y;
    const big = x >= y ? x : y;
    if (m !== m) nanSeen++;
    if (Object.is(m, -0)) negZero++;
    floats += (m === m && Math.abs(m) !== Infinity ? m : 0) + (big > 0 && big !== Infinity ? 1 : 0);
    const name = names[i % names.length];
    const picked = name === null ? 'null' : name;
    const flag = (i & 1) === 0;
    const tagged = flag ? picked : name;
    tags += typeof tagged === 'string' ? tagged.length : (tagged === undefined ? 2 : 1);
    const edge = edges[i & 1];
    const out = edge.output();
    out.value = edge.input().value + (i & 3);
    tags += out.value;
    const maybe = (i % 5 === 0) ? null : edge;
    tags += maybe === null ? 0 : maybe.direction;
  }
  return [ints, floats, tags, nanSeen, negZero, edges[0].v2.value, edges[1].v1.value].join(',');
}
let last = '';
for (let round = 0; round < 40; round++) last = picks(5000 + round);
console.log(last);
