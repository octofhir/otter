const o = { "\ud800": 1 };
const k = Object.keys(o)[0];
console.log(k.length, k.charCodeAt(0).toString(16), o["\ud800"], o["�"], "\ud800" in o, "�" in o);
const p = {}; p["\udc00x"] = 2; p["�x"] = 3;
console.log(Object.keys(p).length, p["\udc00x"], p["�x"], JSON.stringify(Object.keys(p).map(s => s.charCodeAt(0))));
const m = new Map([["\ud800", 1]]);
console.log(m.get("\ud800"), m.get("�"));
const q = {}; const key = String.fromCharCode(0xd83d); q[key] = 5;
console.log(q[key], Object.getOwnPropertyNames(q)[0].charCodeAt(0).toString(16), Reflect.has(q, key), Object.hasOwn(q, key));
