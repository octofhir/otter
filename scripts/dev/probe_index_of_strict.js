// Array.prototype.indexOf: by identity for objects, symbols, booleans, null and
// undefined, by value for strings (built and literal) and numbers (NaN, -0),
// from-index forms, holes before and after the match, an inherited index
// filling a hole, and a length changed by fromIndex; hot loops reach the
// compiled leaf.
var out = [];
var a = {}, b = {}, sym = Symbol('s');
var arr = [1, 'x', a, null, undefined, true, sym, b, a, -0, NaN];
function io(v, f) { return arguments.length > 1 ? arr.indexOf(v, f) : arr.indexOf(v); }
out.push([io(a), io(b), io(sym), io(null), io(undefined), io(true), io(false), io({}), io(a, 3), io(a, -3), io(a, 100), io(a, -100)].join());
out.push([io(0), io(-0), io(NaN), io('x'), io(1)].join());
var holes = [a, , b];
out.push([holes.indexOf(b), holes.indexOf(a), holes.indexOf(undefined)].join());
Array.prototype[1] = b;
out.push('inherited ' + [holes.indexOf(b), holes.indexOf(a)].join());
delete Array.prototype[1];
var grow = [a, b];
out.push('from shrinks ' + grow.indexOf(b, { valueOf: function () { grow.length = 1; return 0; } }));
var objs = []; for (var i = 0; i < 300; i++) objs.push({ i: i });
var hot = 0;
for (var k = 0; k < 20000; k++) hot += objs.indexOf(objs[k % 300]) + objs.indexOf(a);
out.push('hot ' + hot);
var kw = ['break', 'case', 'catch', 'continue', 'debugger', 'default', 'do', 'else', 'finally', 'for', 'function', 'if', 'return', 'switch', 'throw', 'try', 'var', 'while', 'with', 'null', 'true', 'false', 'in', 'instanceof', 'typeof', 'void', 'delete', 'new', 'this'];
var words = ['for', 'x', 'this', 'returns', 'retur' + 'n', 'nul', 'null'];
var nums = [1, 2.5, -0, NaN, 7, 1e9, 3];
var hot2 = 0;
for (var q = 0; q < 30000; q++) { hot2 += kw.indexOf(words[q % 7]) + nums.indexOf([0, NaN, 2.5, 3, 8][q % 5]); }
out.push('hot2 ' + hot2);
out.push('call ' + [].indexOf.call({ length: 2, 0: a, 1: b }, b));
console.log(out.join('\n'));
