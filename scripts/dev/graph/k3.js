function P(v) { this.a = v; this.g = v + 6; }
let bad = 0;
let previous = new P(0);
for (let i = 0; i < 200; i++) {
  const wide = new P(i);
  if (wide.g !== i + 6 || wide.a !== i || previous.g !== previous.a + 6) {
    bad++;
    console.log("FAIL", i, wide.g, wide.a, previous.g, previous.a);
  }
  previous = wide;
}
console.log("bad", bad);
