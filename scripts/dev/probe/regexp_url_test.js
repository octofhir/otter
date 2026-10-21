var urls = ['uggc://jjj.snprobbx.pbz/ybtva.cuc', 'uggc://jjj.snprobbx.pbz/ybtva.cu', 'uggc://jjj.snprobbx.p z/ybtva.cuc'];
var re1 = /(((\w+):\/\/)([^\/:]*)(:(\d+))?)?([^#?]*)(\?([^#]*))?(#(.*))?/;
function t(n) { var s = 0; for (var i = 0; i < n; i++) s += re1.test(urls[i % 3]) ? 1 : 0; return s; }
var t0 = performance.now(); t(20000000); console.log((performance.now() - t0).toFixed(0));
