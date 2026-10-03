function factory(wide) { function Instance(value) { this.a = value; if (wide) { this.b = value + 1; this.c = value + 2; this.d = value + 3; this.e = value + 4; this.f = value + 5; this.g = value + 6; } } return Instance; }
let bad = 0;
function inner(Wide, Small, batch) {
  let previous = new Wide(batch);
  for (let i = 0; i < 80; i++) {
    const wide = new Wide(i); const small = new Small(i);
    if (wide.g !== i + 6 || Object.keys(small).length !== 1 || previous.g !== previous.a + 6) { bad++; }
    previous = wide;
  }
}
for (let batch = 0; batch < 30; batch++) {
  const Wide = factory(true); const Small = factory(false);
  inner(Wide, Small, batch);
}
console.log("bad", bad);
