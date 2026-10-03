function factory(wide) {
  function Instance(value) {
    this.a = value;
    if (wide) { this.b = value + 1; this.c = value + 2; this.d = value + 3; this.e = value + 4; this.f = value + 5; this.g = value + 6; }
  }
  return Instance;
}
let checksum = 0;
for (let batch = 0; batch < 80; batch++) {
  const Wide = factory(true);
  const Small = factory(false);
  let previous = new Wide(batch);
  for (let i = 0; i < 80; i++) {
    const wide = new Wide(i);
    const small = new Small(i);
    if (Object.keys(wide).length !== 7) { console.log("K", batch, i, Object.keys(wide).length); break; }
    if (Object.keys(small).length !== 1) { console.log("S", batch, i); break; }
    if (!(wide instanceof Wide)) { console.log("I", batch, i); break; }
    if (!(small instanceof Small)) { console.log("J", batch, i); break; }
    if (previous.g !== previous.a + 6) { console.log("B", batch, i, previous.g, previous.a); break; }
    previous = wide;
    checksum += wide.g + small.a;
  }
}
console.log(checksum);
