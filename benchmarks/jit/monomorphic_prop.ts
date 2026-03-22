// JIT benchmark: single-shape property load/store
// Measures: shape guard efficiency, inline property access, IC monomorphic fast path

interface Point {
  x: number;
  y: number;
  z: number;
}

function createPoint(x: number, y: number, z: number): Point {
  return { x, y, z };
}

function benchPropRead(points: Point[], n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const p = points[i % points.length];
    sum += p.x + p.y + p.z;
  }
  return sum;
}

function benchPropWrite(points: Point[], n: number): void {
  for (let i = 0; i < n; i++) {
    const p = points[i % points.length];
    p.x = i;
    p.y = i + 1;
    p.z = i + 2;
  }
}

function benchPropReadWrite(points: Point[], n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const p = points[i % points.length];
    sum += p.x;
    p.x = p.y;
    p.y = p.z;
    p.z = sum & 0xff;
  }
  return sum;
}

// All points have the same shape
const points: Point[] = [];
for (let i = 0; i < 100; i++) {
  points.push(createPoint(i, i * 2, i * 3));
}

const N = 1_000_000;
const ITERS = 50;

const start = Date.now();
for (let iter = 0; iter < ITERS; iter++) {
  benchPropRead(points, N);
  benchPropWrite(points, N);
  benchPropReadWrite(points, N);
}
const elapsed = Date.now() - start;
console.log(`monomorphic_prop: ${elapsed}ms (${ITERS} iterations, N=${N})`);
