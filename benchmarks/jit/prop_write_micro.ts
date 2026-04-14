function bench(n: number) {
  const p = { x: 0, y: 0, z: 0 };
  for (let i = 0; i < n; i++) {
    p.x = i;
    p.y = i;
    p.z = i;
  }
}

const N = 10000000;
const start = Date.now();
bench(N);
const end = Date.now();
console.log(`Prop write micro: ${end - start}ms (N=${N})`);
