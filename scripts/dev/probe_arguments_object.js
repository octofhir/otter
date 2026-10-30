// Arguments objects as their single allocation builds them: mapped and
// unmapped kinds, aliasing both ways, descriptors, callee, @@iterator,
// toString tag, extension, deletion, and many arities, also when hot.
var out = [];
function log(label, v) { out.push(label + ' ' + v); }
function desc(o, k) { var d = Object.getOwnPropertyDescriptor(o, k); return d ? JSON.stringify([d.writable, d.enumerable, d.configurable, typeof d.value, typeof d.get]) : 'none'; }
function sloppy(a, b) { var args = arguments; a = 'A'; args[1] = 'B'; return [args[0], b, args.length, args.callee === sloppy, Object.prototype.toString.call(args), desc(args, 0), desc(args, 'length'), desc(args, 'callee'), Array.from(args).join('|')].join(';'); }
function strict(a, b) { 'use strict'; var args = arguments; a = 'A'; args[1] = 'B'; var c; try { c = args.callee; } catch (e) { c = e.constructor.name; } return [args[0], b, args.length, c, Object.prototype.toString.call(args), desc(args, 0), desc(args, 'callee'), [...args].join('|')].join(';'); }
function noFormals() { return [arguments.length, Object.keys(arguments).join(','), JSON.stringify(arguments)].join(';'); }
function defaults(a, b = 2) { a = 9; return [arguments[0], arguments.length].join(); }
function extend() { arguments.extra = 1; arguments[5] = 'x'; delete arguments[0]; return [Object.keys(arguments).join(','), arguments.length, 0 in arguments].join(';'); }
function iter() { return typeof arguments[Symbol.iterator] + ' ' + (arguments[Symbol.iterator] === Array.prototype.values); }
function escapes() { return arguments; }
var e1 = escapes(1, 2, 3), e2 = escapes('a');
log('sloppy', sloppy(1, 2, 3));
log('strict', strict(1, 2, 3));
log('noFormals', noFormals(1, 'x', null));
log('defaults', defaults(1));
log('extend', extend(1, 2));
log('iter', iter());
log('escapes', [e1.length, e1[2], e2[0], e1 !== e2, Object.getPrototypeOf(e1) === Object.prototype].join());
for (var n = 0; n < 12; n++) { var a = escapes.apply(null, Array(n).fill(n)); if (a.length !== n || (n && a[n - 1] !== n)) log('arity', n + ' bad'); }
log('arity', 'ok');
var hot = 0;
for (var i = 0; i < 30000; i++) { var r = escapes(i, i + 1); hot += r[0] + r[1] + r.length; var s = sloppy(i, i); hot += s.length; }
log('hot', hot);
console.log(out.join('\n'));
