// Map/Set insert, lookup, delete, and iteration.
const N = 30000;
const map = new Map();
const set = new Set();
for (let i = 0; i < N; i++) {
  const key = "k" + (i & 8191);
  map.set(key, (i * 17) & 65535);
  set.add(key);
}

let acc = 0;
for (let r = 0; r < 35; r++) {
  for (let i = 0; i < N; i++) {
    const key = "k" + ((i * 13 + r) & 8191);
    if (set.has(key)) acc = (acc + map.get(key)) | 0;
    if ((i & 127) === 0) {
      map.delete(key);
      map.set(key, (acc + i) & 65535);
    }
  }
  for (const value of map.values()) acc = (acc ^ value) | 0;
}
console.log(acc >>> 0);
