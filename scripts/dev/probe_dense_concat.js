// Array.prototype.concat over dense arrays and primitives against the cases
// that must keep the specification steps: holes, @@isConcatSpreadable on an
// instance or a prototype, subclasses, array-likes and nested arrays.
var out = [];
function show(a) { return JSON.stringify(a) + ':' + a.length; }
out.push(show([1, 2].concat([3, 4], 5, 'six', [[7]], null, undefined)));
out.push(show([1.5].concat([{ a: 1 }], [2.5])));
var holey = [1, , 3]; var c = [0].concat(holey); out.push(show(c), 2 in c);
var spread = [1, 2]; spread[Symbol.isConcatSpreadable] = false; out.push(show([0].concat(spread)));
var like = { length: 2, 0: 'a', 1: 'b' }; like[Symbol.isConcatSpreadable] = true; out.push(show([0].concat(like)));
out.push(show([0].concat({ length: 1, 0: 'x' })));
class Sub extends Array {}
var s = Sub.of(1, 2).concat([3]); out.push(s instanceof Sub, show(s));
Array.prototype[Symbol.isConcatSpreadable] = false; out.push(show([1].concat([2]))); delete Array.prototype[Symbol.isConcatSpreadable];
Array.prototype[3] = 'p'; var h = [1, , ].concat([]); out.push(show(h)); delete Array.prototype[3];
var big = []; for (var i = 0; i < 5000; i++) big = big.concat([{ i: i }]);
var sum = 0; for (var i = 0; i < big.length; i++) sum += big[i].i;
out.push(big.length, sum);
console.log(out.join('\n'));
