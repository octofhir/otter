// Property keys holding unpaired surrogates stay distinct from U+FFFD and
// from each other, and enumerate with their exact code units.
const hi = String.fromCharCode(0xD800);
const lo = String.fromCharCode(0xDC00);
const pua = String.fromCharCode(0xE000);
const fffd = "�";
const units = (s) => Array.from(s, (c) => c.charCodeAt(0).toString(16)).join(",");
const report = [];
const check = (label, ok) => {
  let passed;
  try { passed = typeof ok === "function" ? ok() : ok; } catch (e) { passed = false; label += ` (${e})`; }
  report.push(`${passed ? "ok" : "FAIL"} ${label}`);
};

const o = {};
o[hi] = 1;
check("fffd absent", o[fffd] === undefined);
o[fffd] = 2;
o[lo] = 3;
o[pua] = 4;
o[pua + String.fromCharCode(0xE001)] = 5;
check("hi kept", o[hi] === 1);
check("lo distinct", o[lo] === 3);
check("pua", o[pua] === 4);
check("pua pair", o[pua + String.fromCharCode(0xE001)] === 5);
check("keys", Object.keys(o).map(units).join(" ") === "d800 fffd dc00 e000 e000,e001");
check("in", hi in o && !((hi + lo) in o));
check("hasOwn", Object.hasOwn(o, lo));
check("delete", delete o[lo] && !(lo in o) && o[fffd] === 2);

const literal = { "\uD800": "a", "�": "b" };
check("literal distinct", literal[hi] === "a" && literal[fffd] === "b");
check("literal keys", Object.keys(literal).map(units).join(" ") === "d800 fffd");
check("member literal", literal["\uD800"] === "a");

class K { "\uDBFF"() { return 7; } }
check("class method", () => new K()[String.fromCharCode(0xDBFF)]() === 7);
check("method name", () => units(K.prototype[String.fromCharCode(0xDBFF)].name) === "dbff");

const { "\uD800": picked } = literal;
check("destructure", picked === "a");

const parsed = JSON.parse('{"\\ud800":1,"\\ufffd":2}');
check("json parse", parsed[hi] === 1 && parsed[fffd] === 2);
check("json stringify", JSON.stringify({ [hi]: 1 }) === '{"\\ud800":1}');

const forIn = [];
for (const k in { [hi]: 1, [lo]: 2 }) forIn.push(units(k));
check("for-in", forIn.join(" ") === "d800 dc00");
check("ownKeys", Reflect.ownKeys({ [lo]: 1 }).map(units).join() === "dc00");
check("entries", units(Object.entries({ [hi]: 1 })[0][0]) === "d800");
check("defineProperty", (() => {
  const d = {};
  Object.defineProperty(d, hi, { value: 1, enumerable: true });
  return d[fffd] === undefined && Object.getOwnPropertyDescriptor(d, hi).value === 1;
})());
check("fromEntries", Object.fromEntries([[hi, 1]])[fffd] === undefined);

const dict = {};
for (let i = 0; i < 200; i++) dict["k" + i] = i;
dict[hi] = "h";
check("dictionary", dict[hi] === "h" && dict[fffd] === undefined);

const fn = function () {};
fn[hi] = 1;
check("function prop", fn[fffd] === undefined && fn[hi] === 1);

let hot = 0;
const sites = [hi, fffd, lo];
const holder = { [hi]: 1, [fffd]: 2, [lo]: 3 };
for (let i = 0; i < 30000; i++) hot += holder[sites[i % 3]];
check("hot keyed", hot === 60000);

console.log(report.join("\n"));
