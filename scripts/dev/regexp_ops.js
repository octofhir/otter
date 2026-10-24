// The three hottest operation kinds of the regexp fixture's first block.
function Exec(re, string) {
  var sum = 0;
  re.lastIndex = 0;
  var array = re.exec(string);
  if (array) for (var i = 0; i < array.length; i++) { var s = array[i]; if (s) sum += s.length; }
  return sum;
}
const re0 = /^ba/;
const re1 = /(((\w+):\/\/)([^\/:]*)(:(\d+))?)?([^#?]*)(\?([^#]*))?(#(.*))?/;
const re2 = /^\s*|\s*$/g;
const s0 = [], s1 = [], s2 = [];
for (let i = 0; i < 1000; i++) { s0.push('pyvpx' + i); s1.push('uggc://jjj.snprobbx.pbz/ybtva.cuc' + i); s2.push('QBZPbageby_cynprubyqre' + i); }
function bench(name, f) {
  for (let w = 0; w < 3; w++) f();
  const t = performance.now(); let s = 0;
  for (let r = 0; r < 200; r++) s += f();
  console.log(name, (performance.now() - t).toFixed(1), 'ms', s);
}
bench('anchored-miss', () => { let s = 0; for (let i = 0; i < 1000; i++) s += Exec(re0, s0[i]); return s; });
bench('url-groups', () => { let s = 0; for (let i = 0; i < 1000; i++) s += Exec(re1, s1[i]); return s; });
bench('trim-replace', () => { let s = 0; for (let i = 0; i < 1000; i++) s += s2[i].replace(re2, '').length; return s; });
bench('test-only', () => { let s = 0; for (let i = 0; i < 1000; i++) s += re0.test(s0[i]) ? 1 : 0; return s; });
