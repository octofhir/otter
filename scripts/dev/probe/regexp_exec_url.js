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
var urls = ['uggc://jjj.snprobbx.pbz/ybtva.cuc', 'uggc://jjj.snprobbx.pbz/ybtva.cu', 'uggc://jjj.snprobbx.p z/ybtva.cuc'];
var re1 = /(((\w+):\/\/)([^\/:]*)(:(\d+))?)?([^#?]*)(\?([^#]*))?(#(.*))?/;
function run(n) { var s = 0; for (var i = 0; i < n; i++) s += Exec(re1, urls[i % 3]); return s; }
var t0 = performance.now();
var s = run(5000000);
console.log(s, (performance.now() - t0).toFixed(0));
