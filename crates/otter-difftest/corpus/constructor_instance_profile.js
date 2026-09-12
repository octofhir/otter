// Sibling closures share code but construct different layouts. Keep earlier
// instances live while receiver/slab allocation moves young constructors.
function factory(wide) {
  function Instance(value) {
    this.a = value;
    if (wide) {
      this.b = value + 1;
      this.c = value + 2;
      this.d = value + 3;
      this.e = value + 4;
      this.f = value + 5;
      this.g = value + 6;
    }
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
    if (wide.g !== i + 6 || Object.keys(wide).length !== 7 ||
        Object.keys(small).length !== 1 || !(wide instanceof Wide) ||
        !(small instanceof Small) || previous.g !== previous.a + 6) {
      throw new Error("constructor profile changed instance semantics");
    }
    previous = wide;
    checksum += wide.g + small.a;
  }
}
if (checksum !== 544000) throw new Error("incorrect checksum");
console.log(checksum);
