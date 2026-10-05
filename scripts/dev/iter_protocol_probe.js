// Patched built-in iterator protocol must be observed (§7.4.2-§7.4.11).
console.log("start");
const out = [];
const AIP = Object.getPrototypeOf([][Symbol.iterator]());
AIP.return = function () { out.push("return"); return {}; };
{ const [a] = [1, 2]; out.push(a); }
delete AIP.return;
const origNext = AIP.next;
AIP.next = function () { out.push("next"); return origNext.call(this); };
{ const [a, b] = [3, 4]; out.push(a, b); }
for (const x of [5]) out.push(x);
AIP.next = origNext;
const MIP = Object.getPrototypeOf(new Map()[Symbol.iterator]());
const mNext = MIP.next;
MIP.next = function () { out.push("mnext"); return mNext.call(this); };
for (const [k] of new Map([[6, 0]])) out.push(k);
MIP.next = mNext;
const it = [7, 8][Symbol.iterator]();
it.next = function () { out.push("own"); return { done: true }; };
for (const x of { [Symbol.iterator]: () => it }) out.push(x);
Array.prototype[Symbol.iterator] = function* () { yield 9; };
{ const [a] = [1]; out.push(a); }
out.push([...[0, 0]].join(""));
console.log(out.join(","));
