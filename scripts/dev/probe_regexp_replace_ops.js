// Per-call cost of the regexp fixture's dominant `.replace(re, '')` forms,
// hot: a miss on a short subject, a global hit, a trim and a split.
const re14 = /\s+/g, re15 = /^\s*(\S*(\s+\S+)*)\s*$/, re30 = /%2S/gi, re40 = / /g;
const words = ['purpx', 'pvgl', 'qrpe fyvqrgrkg', 'cnerag puebzr6 fvatyr1 gno fryrpgrq', 'tzk%2Subzrcntr%2Sfgneg%2Sqr%2S'];
function bench(name, f) {
  const t = performance.now(); let s = 0;
  for (let r = 0; r < 400000; r++) s += f(words[r % 5]);
  console.log(name.padEnd(12), ((performance.now() - t) * 1e6 / 400000).toFixed(0), 'ns', s);
}
bench('ws-global', (w) => w.replace(re14, '').length);
bench('trim-groups', (w) => w.replace(re15, '').length);
bench('pct-gi', (w) => w.replace(re30, '').length);
bench('space', (w) => w.replace(re40, '').length);
bench('literal-re', (w) => w.replace(/[A-Z]/g, '').length);
bench('split', (w) => w.split(re40).length);
bench('match-gi', (w) => (w.match(/f/gi) || []).length);
