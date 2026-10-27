// Top RegExp patterns by top-level call count in a script, per operation.
// usage: node scripts/dev/regexp_pattern_census.js <script.js> [top]
const counts = new Map();
let depth = 0;
function note(op, re, subject) {
  if (depth > 0 || !(re instanceof RegExp)) return;
  const key = op + ' /' + re.source + '/' + re.flags;
  const c = counts.get(key) ?? { n: 0, len: 0, hit: 0 };
  c.n++; c.len += String(subject).length; counts.set(key, c);
}
const wrap = (owner, name, op, recv) => {
  const orig = owner[name];
  owner[name] = function (a, ...rest) {
    const [re, subject] = recv ? [this, a] : [a, this];
    note(op, re, subject);
    depth++;
    try { return orig.call(this, a, ...rest); } finally { depth--; }
  };
};
wrap(String.prototype, 'replace', 'replace', false);
wrap(String.prototype, 'match', 'match', false);
wrap(String.prototype, 'split', 'split', false);
wrap(RegExp.prototype, 'exec', 'exec', true);
wrap(RegExp.prototype, 'test', 'test', true);
process.on('exit', () => {
  const top = Number(process.argv[3] ?? 25);
  [...counts].sort((a, b) => b[1].n - a[1].n).slice(0, top)
    .forEach(([k, c]) => console.error(String(c.n).padStart(8), (c.len / c.n).toFixed(0).padStart(5), k.slice(0, 110)));
});
require(require('path').resolve(process.argv[2]));
