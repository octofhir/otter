// Indexed stores that grow an ordinary array — reverse fills into an empty
// array, appends, gaps, int32 values into double storage — next to every form
// that must keep the complete `[[Set]]`: a non-writable length, a frozen or
// non-extensible receiver, an inherited indexed setter or read-only index, a
// sparse-sized gap and a custom prototype. Hot loops tier the sites up; the
// descriptors and key order of every result are compared.
function reverseFill(n) {
  const out = new Array();
  let i = n;
  while (--i >= 0) out[i] = 0;
  return out;
}
function appendInts(n) {
  const out = [];
  for (let i = 0; i < n; i++) out[i] = (i * 7) & 0xff;
  return out;
}
function mixIntoDoubles(values, n) {
  for (let i = 0; i < n; i++) values[i] = i & 1 ? i * 3 : i + 0.5;
  return values;
}
function gapStore(n) {
  const out = [1];
  out[n] = 2;
  return out;
}
function am(src, dst, n) {
  let c = 0;
  for (let i = 0; i < n; i++) {
    const l = src[i] * 3 + dst[i] + c;
    c = l >> 28;
    dst[i] = l & 0xfffffff;
  }
  return c;
}

function describe(o) {
  const keys = Object.keys(o);
  const attrs = keys.map((k) => {
    const d = Object.getOwnPropertyDescriptor(o, k);
    return (d.writable ? "w" : "") + (d.enumerable ? "e" : "") + (d.configurable ? "c" : "");
  });
  return o.length + ":" + keys.join(",") + "|" + Array.from(new Set(attrs)).join(",");
}

let checksum = 0;
for (let round = 0; round < 2000; round++) {
  const r = reverseFill(8 + (round & 7));
  checksum = (checksum + r.length) | 0;
  const a = appendInts(16);
  checksum = (checksum + a[15]) | 0;
  const d = mixIntoDoubles([0.25, 0.5, 0.75, 1.25], 6);
  checksum = (checksum + d.length + d[5]) | 0;
  const w = reverseFill(6);
  checksum = (checksum + am(appendInts(6), w, 6)) | 0;
  checksum = (checksum + w[5]) | 0;
}

const results = [checksum, describe(reverseFill(5)), describe(appendInts(4))];
results.push(describe(mixIntoDoubles([0.5, 1.5], 5)), JSON.stringify(mixIntoDoubles([0.5, 1.5], 5)));
results.push(describe(gapStore(4)), 2 in gapStore(4), describe(gapStore(5000)));

const fixed = [1, 2];
Object.defineProperty(fixed, "length", { writable: false });
try { (function () { "use strict"; fixed[2] = 3; })(); results.push("no throw"); } catch (e) { results.push(e.name); }
fixed[5] = 9;
results.push(describe(fixed));

const frozen = Object.freeze([1, 2]);
frozen[2] = 3;
results.push(describe(frozen));
const sealed = Object.preventExtensions([4, 5]);
sealed[3] = 1;
results.push(describe(sealed));

const custom = Object.setPrototypeOf([], { set 2(v) { results.push("proto setter " + v); } });
custom[2] = 7;
results.push(describe(custom));

let hits = 0;
Object.defineProperty(Array.prototype, "3", { set(v) { hits += v; }, get() { return "inh"; }, configurable: true });
const viaSetter = appendInts(6);
results.push(hits, describe(viaSetter), viaSetter[3]);
delete Array.prototype[3];
Object.defineProperty(Array.prototype, "1", { value: "ro", writable: false, configurable: true });
const readOnly = reverseFill(3);
results.push(describe(readOnly), readOnly[1]);
delete Array.prototype[1];
results.push(describe(reverseFill(3)), am(appendInts(4), reverseFill(4), 4));
console.log(JSON.stringify(results));
