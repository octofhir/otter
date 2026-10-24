// Megamorphic constructor stores: one base constructor initializes fields
// on receivers of many subclass shapes (the TypeScript AST pattern), then
// polymorphic reads of those fields.
function Base(kind) { this.kind = kind; this.flags = 0; this.start = -1; this.end = -1; this.parent = null; }
const classes = [];
for (let i = 0; i < 64; i++) {
  const C = new Function('Base', `return function C${i}(k) { Base.call(this, k); this.extra${i} = ${i}; }`)(Base);
  C.prototype = Object.create(Base.prototype);
  classes.push(C);
}
let sum = 0;
const t0 = Date.now();
for (let r = 0; r < 60000; r++) {
  for (let i = 0; i < 64; i++) {
    const o = new classes[i](i);
    sum += o.kind + o.flags + o.start;
  }
}
const ms = Date.now() - t0;
console.log(sum, ms + 'ms');
