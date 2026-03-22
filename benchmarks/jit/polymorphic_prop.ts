// JIT benchmark: 2-4 shape property access
// Measures: polymorphic IC dispatch, guard chains, shape diversity handling

interface Shape2D {
  x: number;
  y: number;
}

interface Shape3D {
  x: number;
  y: number;
  z: number;
}

interface Shape4D {
  x: number;
  y: number;
  z: number;
  w: number;
}

function getX(obj: Shape2D | Shape3D | Shape4D): number {
  return obj.x;
}

function getY(obj: Shape2D | Shape3D | Shape4D): number {
  return obj.y;
}

function sumXY(obj: Shape2D | Shape3D | Shape4D): number {
  return obj.x + obj.y;
}

// Create objects with 2 distinct shapes
const objects2: (Shape2D | Shape3D)[] = [];
for (let i = 0; i < 100; i++) {
  if (i % 2 === 0) {
    objects2.push({ x: i, y: i * 2 });
  } else {
    objects2.push({ x: i, y: i * 2, z: i * 3 });
  }
}

// Create objects with 3 distinct shapes
const objects3: (Shape2D | Shape3D | Shape4D)[] = [];
for (let i = 0; i < 100; i++) {
  if (i % 3 === 0) {
    objects3.push({ x: i, y: i * 2 });
  } else if (i % 3 === 1) {
    objects3.push({ x: i, y: i * 2, z: i * 3 });
  } else {
    objects3.push({ x: i, y: i * 2, z: i * 3, w: i * 4 });
  }
}

const N = 1_000_000;
const ITERS = 50;

function bench2Shapes(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum += getX(objects2[i % objects2.length]);
    sum += getY(objects2[i % objects2.length]);
  }
  return sum;
}

function bench3Shapes(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum += sumXY(objects3[i % objects3.length]);
  }
  return sum;
}

const start = Date.now();
for (let iter = 0; iter < ITERS; iter++) {
  bench2Shapes(N);
  bench3Shapes(N);
}
const elapsed = Date.now() - start;
console.log(`polymorphic_prop: ${elapsed}ms (${ITERS} iterations, N=${N})`);
