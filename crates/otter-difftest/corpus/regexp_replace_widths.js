// Hot RegExp replacement and exec over one-byte and two-byte subjects,
// templates and replacer results: every `$` form, results that widen
// midway, a replaced @@replace or exec, and lastIndex that is read-only,
// coerces through valueOf, or points past the subject.
const subjects = [
  "alpha beta gamma delta",
  "café crème brûlée",
  "őrülő 中文 abc",
  "emoji \u{1F600} mid \u{1F601} end",
  "",
];
const templates = ["[$&]", "<$1|$2>", "$`#", "#$'", "$$", "$<w>!", "ő$1", "ÿ", "$0$9$10"];
const patterns = [/(\w)(\w*)/g, /(?<w>[a-zà-ÿ]+)/g, /\s/, /(.)\1?/gu, /x*/g];

function replaceAll(rounds) {
  const out = [];
  for (let r = 0; r < rounds; r++) {
    for (const s of subjects) {
      for (const re of patterns) {
        for (const t of templates) {
          const v = s.replace(re, t);
          if (r === rounds - 1) out.push(v);
        }
        const f = s.replace(re, (m, a, b, i) => (i & 1 ? "中" + m : m.toUpperCase()));
        if (r === rounds - 1) out.push(f);
      }
    }
  }
  return out.join("|");
}

// A RegExp whose exec or @@replace is replaced mid-loop.
function patched(n) {
  let s = "";
  const re = /b/g;
  for (let i = 0; i < n; i++) {
    if (i === n >> 2) re.exec = function (str) { this.lastIndex = 0; return null; };
    if (i === n >> 1) delete re.exec;
    if (i === (3 * n) >> 2) RegExp.prototype[Symbol.replace] = function () { return "patched"; };
    s = "abcb".replace(re, "X");
  }
  delete RegExp.prototype[Symbol.replace];
  return s;
}

// exec and lastIndex: read-only, coercing, out of range, sticky.
function lastIndexCases() {
  const out = [];
  const g = /a/g;
  for (let i = 0; i < 2000; i++) g.exec("aaa");
  out.push(g.lastIndex);
  const frozen = /a/g;
  Object.defineProperty(frozen, "lastIndex", { writable: false, value: 0 });
  let thrown = 0;
  for (let i = 0; i < 2000; i++) {
    try { frozen.exec("aaa"); } catch (e) { thrown++; }
  }
  out.push(thrown, frozen.lastIndex);
  const coerced = /a/g;
  let reads = 0;
  for (let i = 0; i < 2000; i++) {
    coerced.lastIndex = { valueOf() { reads++; return 1; } };
    const m = coerced.exec("aaa");
    if (!m || m.index !== 1) out.push("bad");
  }
  out.push(reads, coerced.lastIndex);
  const far = /a/g;
  for (let i = 0; i < 2000; i++) { far.lastIndex = 10; far.exec("aaa"); }
  out.push(far.lastIndex);
  const sticky = /a/y;
  let hits = 0;
  for (let i = 0; i < 2000; i++) { sticky.lastIndex = i % 4; if (sticky.exec("aab")) hits++; }
  out.push(hits, sticky.lastIndex);
  return out.join(",");
}

console.log(replaceAll(30));
console.log(patched(8000));
console.log(lastIndexCases());
