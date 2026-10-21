function F() {}
F.calls = 5;
Object.defineProperty(F, "calls", { writable: false });
const d = Object.getOwnPropertyDescriptor(F, "calls");
console.log(F.calls, d.value, d.writable, d.enumerable, d.configurable);
const o = { calls: 5 };
Object.defineProperty(o, "calls", { writable: false });
console.log(o.calls);
function G() {}
G.x = 1;
Object.defineProperty(G, "x", { enumerable: false });
console.log(G.x, Object.getOwnPropertyDescriptor(G, "x").enumerable);
function H() {}
Object.defineProperty(H, "acc", { configurable: true, enumerable: true, get() { return 7; } });
Object.defineProperty(H, "acc", { enumerable: false });
const ad = Object.getOwnPropertyDescriptor(H, "acc");
console.log(H.acc, typeof ad.get, ad.enumerable, ad.configurable, "value" in ad);
Object.defineProperty(H, "name", { value: "renamed" });
console.log(H.name, Object.getOwnPropertyDescriptor(H, "name").configurable);
Object.defineProperty(H, "fresh", { value: 3 });
const fd = Object.getOwnPropertyDescriptor(H, "fresh");
console.log(fd.value, fd.writable, fd.enumerable, fd.configurable);
