// Complete literal scans still enter the same JS exec/test/string protocols.
let sum = 0;
for (let i = 0; i < 1500; i++) {
  const text = "ab=aba=" + i + "éĀΩ";
  const re = /aba/dg;
  const m = re.exec(text);
  if (m.index !== 3 || m.indices[0][0] !== 3 || m.indices[0][1] !== 6 ||
      m.input !== text || m.length !== 1 || m.groups !== undefined || re.lastIndex !== 6) {
    throw new Error("literal result");
  }
  if (re.exec(text) !== null || re.lastIndex !== 0) throw new Error("global reset");
  const sticky = /aba/y;
  sticky.lastIndex = 0;
  if (sticky.test(text) || sticky.lastIndex !== 0) throw new Error("sticky partial");
  sticky.lastIndex = 3;
  if (!sticky.test(text) || sticky.lastIndex !== 6) throw new Error("sticky hit");
  sum += text.replace(/=/g, "$&x").length;
  sum += text.split(/=/).length;
  sum += text.search(/ĀΩ/);
  sum += [...text.matchAll(/=/g)].length;
  const wide = /ĀΩ/.exec(text);
  if (wide[0] !== "ĀΩ" || wide.index !== text.length - 2) throw new Error("wide");
}
const modified = /aba/g;
modified.exec = function () { return null; };
if (modified.test("aba")) throw new Error("overridden exec");
const frozen = /a/g;
Object.defineProperty(frozen, "lastIndex", { writable: false });
let threw = false;
try { frozen.exec("a"); } catch (e) { threw = e instanceof TypeError; }
if (!threw) throw new Error("non-writable lastIndex");
const coercing = /aba/g;
coercing.lastIndex = { valueOf() { return 3; } };
if (coercing.exec("ab=aba").index !== 3) throw new Error("lastIndex coercion");
const reused = /aba/;
if (reused.exec("aba")[0] !== "aba") throw new Error("before compile");
reused.compile("=", "g");
if (reused.exec("x=y").index !== 1) throw new Error("after compile");
for (let i = 0; i < 1500; i++) {
  for (const re of [/(a){0}b/, /(?<gone>a){0}b/]) {
    const m = re.exec("b");
    if (m.length !== 2 || m[1] !== undefined) throw new Error("elided capture");
    if (m.groups !== undefined && !("gone" in m.groups)) throw new Error("elided name");
  }
}
const indexedAbsent = /(?<gone>a){0}b/d.exec("b");
if (indexedAbsent.length !== 2 || indexedAbsent.indices.length !== 2 ||
    indexedAbsent.indices[1] !== undefined || indexedAbsent.indices.groups.gone !== undefined ||
    !("gone" in indexedAbsent.indices.groups)) throw new Error("elided capture indices");
console.log(sum, RegExp.lastMatch, RegExp.leftContext, RegExp.rightContext);
