// Bytes per retained object for common shapes; compare engines.
const retained = [];
function used() { globalThis.gc(); globalThis.gc(); return process.memoryUsage().heapUsed; }
function measure(name, make) {
  const n = 200000;
  const keep = new Array(n);
  const before = used();
  for (let i = 0; i < n; i++) keep[i] = make(i);
  const after = used();
  retained.push(keep);
  console.log(name.padEnd(14), ((after - before) / n).toFixed(1));
}
measure("empty {}", () => ({}));
measure("{a,b}", (i) => ({ a: i, b: i }));
measure("{l,r} ptr", (i) => ({ l: null, r: null }));
function Node(k, v) { this.key = k; this.value = v; this.left = null; this.right = null; }
measure("new Node", (i) => new Node(i, null));
measure("[10 ints]", () => [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
measure("string concat", (i) => "String for key " + i + " in leaf node");
measure("double {x}", (i) => ({ x: i + 0.5 }));
measure("closure", (i) => () => i);
