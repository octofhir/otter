// Array.prototype.slice on plain arrays: holes inside and outside the range,
// relative and clamped bounds, non-Number bounds, a species constructor,
// inherited indices filling holes, and small slices of a large array.
var out = [];
function show(label, v) { out.push(label + ' ' + JSON.stringify(v) + ' ' + v.length + ' ' + Object.keys(v).join(',')); }
var big = []; for (var i = 0; i < 5000; i++) big.push(i);
show('tail', big.slice(-3));
show('mid', big.slice(10, 13));
show('empty', big.slice(20, 10));
show('clamp', [1, 2, 3].slice(-10, 10));
var holeOut = [1, 2, , 4, 5];
show('hole outside', holeOut.slice(3));
show('hole inside', holeOut.slice(1, 4));
Array.prototype[2] = 'P';
show('hole inherited', holeOut.slice(1, 4));
show('hole outside inherited', holeOut.slice(3, 5));
delete Array.prototype[2];
show('nan', big.slice(NaN, 2));
show('frac', big.slice(1.7, 3.9));
show('strings', big.slice('2', '4'));
show('valueOf', big.slice({ valueOf: function () { out.push('valueOf'); return 4995; } }));
show('inf', big.slice(-Infinity, Infinity).length);
class MyArray extends Array {}
var mine = MyArray.from([1, 2, 3, 4]);
var ms = mine.slice(1, 3); out.push('species ' + (ms instanceof MyArray) + ' ' + ms.join());
var withLen = [1, 2, 3]; withLen.length = 5;
show('long length', withLen.slice(1));
show('doubles', [1.5, 2.5, 3.5].slice(1));
var stack = [];
var hot = 0;
for (var k = 0; k < 20000; k++) { stack.push(k); var t = stack.slice(-2); hot += t[0] | 0; }
out.push('hot ' + hot);
console.log(out.join('\n'));
