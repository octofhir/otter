// Named loads on primitive receivers through one site: absent names, names
// String/Number/Boolean.prototype and Object.prototype gain and lose later,
// accessors (sloppy and strict `this`), `length` and index-like names.
var out = [];
function read(v, k) { return v[k]; }
function readType(v) { return v.type; }
function readLen(v) { return v.length; }
function readZero(v) { return v['0']; }
function readFive(v) { return v['5']; }
function readNegZero(v) { return v['-0']; }
function readFrac(v) { return v['1.5']; }
var values = ['abc', 42, true, 'longer string here', 3.5, false, ''];
function sweep(label) {
  var row = [];
  for (var i = 0; i < values.length; i++) {
    var v = values[i];
    row.push(String(readType(v)), String(readLen(v)), String(readZero(v)), String(readFive(v)),
      String(readNegZero(v)), String(readFrac(v)), String(read(v, 'toFixed') === undefined));
  }
  out.push(label + ' ' + row.join(','));
}
for (var r = 0; r < 300; r++) sweep('warm');
out.splice(1);
String.prototype.type = 'S'; sweep('string proto type');
Number.prototype.type = 'N'; sweep('number proto type');
delete String.prototype.type; sweep('deleted string type');
Object.prototype.type = 'O'; sweep('object proto type');
delete Object.prototype.type; delete Number.prototype.type;
Object.defineProperty(String.prototype, 'type', { configurable: true, get: function () { return typeof this; } });
sweep('sloppy getter');
Object.defineProperty(String.prototype, 'type', { configurable: true, get: function () { 'use strict'; return typeof this; } });
sweep('strict getter');
delete String.prototype.type;
String.prototype[5] = 'five'; String.prototype['-0'] = 'nz'; String.prototype['1.5'] = 'frac';
sweep('index names on proto');
delete String.prototype[5]; delete String.prototype['-0']; delete String.prototype['1.5'];
Object.defineProperty(Boolean.prototype, 'type', { configurable: true, value: 'B' });
sweep('boolean type');
var hot = 0;
for (var k = 0; k < 50000; k++) { var v = values[k % values.length]; if (readType(v) === 'B') hot++; }
out.push('hot ' + hot);
console.log(out.join('\n'));
