//! Property keys keep every code unit of their string, unpaired surrogates
//! included.
//!
//! # Contents
//! - Keys built at run time, spelled in literals, defined by classes,
//!   parsed and printed by JSON, and enumerated back.
//! - Symbol descriptions and the `Symbol.for` registry keys.
//!
//! # Invariants
//! - A key holding an unpaired surrogate never aliases U+FFFD, the escape
//!   marker U+E000, or any other key, and enumerates with its exact units.
//!
//! # See also
//! - `otter-vm::wtf16_escape` — the injective key spelling.

use otter_runtime::{Runtime, SourceInput};

const SCRIPT: &str = r#"
const hi = String.fromCharCode(0xD800);
const lo = String.fromCharCode(0xDC00);
const pua = String.fromCharCode(0xE000);
const fffd = "�";
const units = (s) => Array.from(s, (c) => c.charCodeAt(0).toString(16)).join(",");
const failures = [];
const check = (label, ok) => {
  let passed;
  try { passed = ok(); } catch (error) { passed = false; label += ` (${error})`; }
  if (!passed) failures.push(label);
};

const o = {};
o[hi] = 1;
check("fffd absent", () => o[fffd] === undefined);
o[fffd] = 2;
o[lo] = 3;
o[pua] = 4;
o[pua + String.fromCharCode(0xE001)] = 5;
check("distinct", () => o[hi] === 1 && o[lo] === 3 && o[pua] === 4
  && o[pua + String.fromCharCode(0xE001)] === 5);
check("keys", () => Object.keys(o).map(units).join(" ") === "d800 fffd dc00 e000 e000,e001");
check("in", () => hi in o && !((hi + lo) in o) && Object.hasOwn(o, lo));
check("delete", () => delete o[lo] && !(lo in o) && o[fffd] === 2);

const literal = { "\uD800": "a", "�": "b" };
check("literal", () => literal[hi] === "a" && literal[fffd] === "b" && literal["\uD800"] === "a");
check("literal keys", () => Object.keys(literal).map(units).join(" ") === "d800 fffd");
const { "\uD800": picked } = literal;
check("destructure", () => picked === "a");

class K { "\uDBFF"() { return 7; } static "\uDC01" = 8; "\uD801" = 9; }
check("class method", () => new K()[String.fromCharCode(0xDBFF)]() === 7);
check("method name", () => units(K.prototype[String.fromCharCode(0xDBFF)].name) === "dbff");
check("class fields", () => K[String.fromCharCode(0xDC01)] === 8
  && new K()[String.fromCharCode(0xD801)] === 9);

check("json parse", () => {
  const parsed = JSON.parse('{"\\ud800":1,"\\ufffd":2}');
  return parsed[hi] === 1 && parsed[fffd] === 2;
});
check("json stringify", () => JSON.stringify({ [hi]: 1 }) === '{"\\ud800":1}');

check("enumeration", () => {
  const seen = [];
  for (const k in { [hi]: 1, [lo]: 2 }) seen.push(units(k));
  return seen.join(" ") === "d800 dc00"
    && Reflect.ownKeys({ [lo]: 1 }).map(units).join() === "dc00"
    && units(Object.entries({ [hi]: 1 })[0][0]) === "d800";
});
check("defineProperty", () => {
  const d = {};
  Object.defineProperty(d, hi, { value: 1, enumerable: true });
  return d[fffd] === undefined && Object.getOwnPropertyDescriptor(d, hi).value === 1
    && Object.fromEntries([[hi, 1]])[fffd] === undefined;
});
check("dictionary", () => {
  const dict = {};
  for (let i = 0; i < 200; i++) dict["k" + i] = i;
  dict[hi] = "h";
  return dict[hi] === "h" && dict[fffd] === undefined;
});
check("function property", () => {
  const fn = function () {};
  fn[hi] = 1;
  return fn[fffd] === undefined && fn[hi] === 1;
});
check("string spread", () => {
  const spread = { ...("x" + hi + "\u{1F600}") };
  return Object.keys(spread).join() === "0,1,2,3" && spread[1] === hi
    && units(spread[2]) === "d83d" && units(spread[3]) === "de00";
});
check("symbols", () => Symbol.for(hi) !== Symbol.for(fffd)
  && Symbol.keyFor(Symbol.for(hi)) === hi && Symbol(lo).description === lo);
check("hot keyed", () => {
  const sites = [hi, fffd, lo];
  const holder = { [hi]: 1, [fffd]: 2, [lo]: 3 };
  let sum = 0;
  for (let i = 0; i < 30000; i++) sum += holder[sites[i % 3]];
  return sum === 60000;
});
failures.join("; ");
"#;

#[test]
fn unpaired_surrogate_keys_keep_their_code_units() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(SCRIPT),
            "<property-key-code-units>",
        )
        .expect("script")
        .completion_string()
        .to_string();
    assert_eq!(completion, "", "failing cases");
}
