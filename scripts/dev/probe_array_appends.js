// Indexed stores that append: at the length, as a builder object tracks it.
function Builder() { this.items = []; this.count = 0; }
Builder.prototype.add = function (v) { this.items[this.count] = v; this.count++; return this; };
let total = 0;
const t = performance.now();
for (let r = 0; r < 20000; r++) {
  const b = new Builder();
  for (let i = 0; i < 50; i++) b.add(i);
  total += b.items.length;
}
const t1 = performance.now();
let total2 = 0;
for (let r = 0; r < 20000; r++) {
  const a = [];
  for (let i = 0; i < 50; i++) a[i] = i;
  total2 += a.length;
}
console.log(total, (t1 - t).toFixed(1), 'ms', total2, (performance.now() - t1).toFixed(1), 'ms');
