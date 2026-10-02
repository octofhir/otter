class Base { constructor(x) { this.x = x; } get() { return this.x; } }
class Derived extends Base { constructor(x) { super(x * 2); this.y = x; } get() { return super.get() + this.y; } }
let acc = 0;
for (let i = 0; i < 3000; i++) { const o = new Derived(i & 15); acc += o.get(); }
console.log(acc);
