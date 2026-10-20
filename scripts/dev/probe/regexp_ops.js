// Per-operation regexp costs (ms per 200k calls) for comparing engines.
function Exec(re, string) {
  var sum = 0;
  re.lastIndex = 0;
  var array = re.exec(string);
  if (array) {
    for (var i = 0; i < array.length; i++) {
      var substring = array[i];
      if (substring) sum += substring.length;
    }
  }
  return sum;
}
var strs = ['pyvpx', 'pyvbx', 'bapyx', 'pyv%x', 'qyvpx'];
var urls = ['uggc://jjj.snprobbx.pbz/ybtva.cuc', 'uggc://jjj.snprobbx.pbz/ybtva.cu', 'uggc://jjj.snprobbx.p z/ybtva.cuc'];
var re0 = /^ba/;
var re1 = /(((\w+):\/\/)([^\/:]*)(:(\d+))?)?([^#?]*)(\?([^#]*))?(#(.*))?/;
var re2 = /^\s*|\s*$/g;
var re3 = /\bQBZPbageby_cynprubyqre\b/;
var alt = /qqqq|qqq|qq|q|ZZZZ|ZZZ|ZZ|Z|llll|ll|l|uu|u|UU|U|zz|z|ff|f|gg|g|sss|ss|s|mmm|mm|m/g;
var N = 200000;
function bench(name, f) {
  var t0 = performance.now(); var s = 0;
  for (var i = 0; i < N; i++) s += f(i);
  console.log(name.padEnd(14), (performance.now() - t0).toFixed(1), s);
}
for (var round = 0; round < 2; round++) {
  bench('exec ^ba', i => Exec(re0, strs[i % 5]));
  bench('exec url', i => Exec(re1, urls[i % 3]));
  bench('replace trim', i => 'QBZPbageby_cynprubyqre'.replace(re2, '').length);
  bench('exec \\b', i => Exec(re3, 'vachggrkg QBZPbageby_cynprubyqre'));
  bench('exec alt/g', i => Exec(alt, 'qqqq, ZZZ q, llll'));
  bench('test', i => re0.test(strs[i % 5]) ? 1 : 0);
  bench('replace g str', i => 'a.b.c.d.e'.replace(/\./g, '_').length);
  bench('split', i => 'a,b,c,d'.split(/,/).length);
}
