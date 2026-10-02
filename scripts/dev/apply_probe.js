function func(...args) { return [...args]; }
let out = [];
for (let i = 0; i < 300; i++) {
  const ta = new Uint8Array([1, 2, 3]);
  out = func.apply(null, ta);
}
console.log(out.join());
const fns = [function(){ return Function.prototype.toString.call(Math.max); }];
console.log(fns[0]().includes("native code"));
console.log([Math.max].map(Function.prototype.toString.call, Function.prototype.toString).length);
