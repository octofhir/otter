// A constructor with more fields than the inline slot capacity and non-scalar
// values, so its receiver takes the reserved-slab preparation path rather than
// the pre-shaped simple-constructor path. The callee tiers up before its
// caller, so the caller's generated construct boundary prepares every receiver
// with an out-of-line slab whose length is still zero when slot zero appends.
function Cell(v) {
  this.a = v; this.b = v + 1; this.c = null; this.d = v * 2; this.e = "x" + (v & 1); this.f = v;
}
function make(v) { return new Cell(v); }
let acc = 0;
let bad = 0;
let keys = "";
for (let i = 0; i < 400; i++) {
  const c = make(i);
  acc += c.a + c.b + c.d + c.f + c.e.length;
  if (c.c !== null || c.a !== i || c.f !== i) bad++;
  keys = Object.keys(c).join(",");
}
JSON.stringify({ acc, bad, keys });
