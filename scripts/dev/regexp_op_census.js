// Census of RegExp-consuming calls in a script: per operation, the call
// count and how many repeat an earlier (subject, regexp source+flags) pair.
// usage: node scripts/dev/regexp_op_census.js <script.js>
const seen = new Map();
const stats = {};
let depth = 0;
function note(op, subject, re) {
  if (depth > 0 || !(re instanceof RegExp) || typeof subject !== 'string') return;
  const key = op + '\0' + re.source + '/' + re.flags + '\0' + subject;
  const s = stats[op] ??= { calls: 0, repeats: 0, units: 0 };
  s.calls++; s.units += subject.length;
  if (seen.has(key)) s.repeats++; else seen.set(key, 1);
}
for (const [owner, name, op] of [[String.prototype, 'replace', 'replace'], [String.prototype, 'match', 'match'], [String.prototype, 'split', 'split'], [String.prototype, 'search', 'search']]) {
  const orig = owner[name];
  owner[name] = function (re, ...rest) { note(op + (re instanceof RegExp && re.global ? '/g' : ''), String(this), re); depth++; try { return orig.call(this, re, ...rest); } finally { depth--; } };
}
for (const name of ['exec', 'test']) {
  const orig = RegExp.prototype[name];
  RegExp.prototype[name] = function (s) { note(name + (this.global ? '/g' : ''), String(s), this); depth++; try { return orig.call(this, s); } finally { depth--; } };
}
process.on('exit', () => { for (const [op, s] of Object.entries(stats)) console.error(op.padEnd(10), s.calls, 'repeats', s.repeats, 'avg-len', (s.units / s.calls).toFixed(1)); });
require(require('path').resolve(process.argv[2]));
