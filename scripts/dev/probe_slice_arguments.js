// Array.prototype.slice over arguments objects: unmodified (mapped and
// unmapped), mutated parameters, modified length/elements, accessors, holes
// from deletes, prototype indices, and non-Number bounds.
var slice = [].slice;
var out = [];
function show(label, v) { out.push(label + ' ' + JSON.stringify(v) + ' ' + Array.isArray(v) + ' ' + v.length); }
function sloppy() { return slice.call(arguments, 1); }
function sloppyAll() { return slice.call(arguments); }
function strict() { 'use strict'; return slice.call(arguments, -2); }
function mutated(a, b) { a = 'A'; b = 'B'; return slice.call(arguments); }
function strictMutated(a) { 'use strict'; a = 'A'; return slice.call(arguments); }
function longer() { arguments.length = 5; return slice.call(arguments); }
function shorter() { arguments.length = 1; return slice.call(arguments); }
function assigned() { arguments[1] = 'x'; return slice.call(arguments); }
function deleted() { delete arguments[0]; return slice.call(arguments); }
function accessor() { Object.defineProperty(arguments, 0, { get: function () { return 'got'; } }); return slice.call(arguments); }
function extra() { arguments[3] = 'four'; return slice.call(arguments); }
function bounds() { return slice.call(arguments, { valueOf: function () { out.push('valueOf'); return 1; } }, '3'); }
function negative() { return slice.call(arguments, -10, -1); }
function empty() { return slice.call(arguments, 1); }
function nested() { var r = slice.call(arguments, 0); r.push('more'); return r; }
show('sloppy', sloppy(1, 2, 3));
show('sloppyAll', sloppyAll('a', 'b'));
show('strict', strict(1, 2, 3, 4));
show('mutated', mutated(1, 2, 3));
show('strictMutated', strictMutated(1, 2));
show('longer', longer(1, 2));
show('shorter', shorter(1, 2, 3));
show('assigned', assigned(1, 2, 3));
show('deleted', deleted(1, 2, 3));
show('accessor', accessor(1, 2));
show('extra', extra(1, 2));
show('bounds', bounds(1, 2, 3, 4, 5));
show('negative', negative(1, 2, 3, 4));
show('empty', empty());
show('nested', nested(7, 8));
Object.prototype[1] = 'proto';
function protoHole() { delete arguments[1]; return slice.call(arguments); }
show('protoHole', protoHole(1, 2, 3));
delete Object.prototype[1];
var hot = 0;
for (var i = 0; i < 20000; i++) hot += sloppy(i, i + 1, i + 2).length + mutated(i, 0).length;
out.push('hot ' + hot);
console.log(out.join('\n'));
