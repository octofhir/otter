// Object/class property access + method dispatch — shape/IC stress.
class Point {
  constructor(x, y) { this.x = x; this.y = y; this.tag = 0; }
  dist2() { return this.x * this.x + this.y * this.y; }
  bump() { this.tag = (this.tag + 1) | 0; return this; }
}
const N = 100_000;
const pts = new Array(N);
for (let i = 0; i < N; i++) pts[i] = new Point(i % 1000, (i * 7) % 1000);

let acc = 0;
for (let r = 0; r < 15; r++) {
  for (let i = 0; i < N; i++) {
    const p = pts[i];
    p.bump();
    acc += p.dist2() + p.x - p.y + p.tag;
  }
}
console.log(acc % 1_000_000_007);
