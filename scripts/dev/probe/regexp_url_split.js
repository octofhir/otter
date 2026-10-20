var urls = ['uggc://jjj.snprobbx.pbz/ybtva.cuc', 'uggc://jjj.snprobbx.pbz/ybtva.cu', 'uggc://jjj.snprobbx.p z/ybtva.cuc'];
var re1 = /(((\w+):\/\/)([^\/:]*)(:(\d+))?)?([^#?]*)(\?([^#]*))?(#(.*))?/;
var N = 3000000;
function t(n) { var s = 0; for (var i = 0; i < n; i++) s += re1.test(urls[i % 3]) ? 1 : 0; return s; }
function e(n) { var s = 0; for (var i = 0; i < n; i++) s += re1.exec(urls[i % 3]) !== null ? 1 : 0; return s; }
function full(n) { var s = 0; for (var i = 0; i < n; i++) { var a = re1.exec(urls[i % 3]); for (var j = 0; j < a.length; j++) { var x = a[j]; if (x) s += x.length; } } return s; }
function arr(n) { var s = 0; var a = ['uggc://jjj.snprobbx.pbz/ybtva.cuc','uggc://jjj.snprobbx.pbz','uggc://','uggc',undefined,undefined,'/ybtva.cuc',undefined,undefined,undefined,undefined,undefined]; for (var i = 0; i < n; i++) { for (var j = 0; j < a.length; j++) { var x = a[j]; if (x) s += x.length; } } return s; }
for (const [name, f] of [['test', t], ['exec', e], ['full', full], ['jsloop', arr]]) {
  var t0 = performance.now(); var r = f(N); console.log(name.padEnd(7), (performance.now() - t0).toFixed(0), r);
}
