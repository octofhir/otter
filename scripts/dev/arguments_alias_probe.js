// Mapped arguments must alias formals exactly where aliasing is observable.
const out = [];
function g(a, b) { return arguments.length + arguments[0]; }
function h(a) { a = 5; return arguments[0]; }
function k(a) { arguments[0] = 7; return a; }
function m(a) { (() => { a = 9; })(); return arguments[0]; }
function s(a) { "use strict"; a = 5; return arguments[0]; }
function mod(x) { x[0] = 3; }
function e(a) { mod(arguments); return a; }
function c(a) { return arguments["cal" + "lee"] === c; }
function v(a) { var a = 2; return arguments[0]; }
function fd(a) { function a() {} return typeof arguments[0]; }
function fi(a) { for (a in { x: 1 }); return arguments[0]; }
function da(a) { [a] = [4]; return arguments[0]; }
function de(a) { delete arguments[0]; return a + ":" + arguments[0]; }
function ar(a) { return (() => arguments[0])(); }
function w(a) { with ({}) { a = 3; } return arguments[0]; }
function inc(a) { a++; return arguments[0]; }
function obj(a) { ({ a } = { a: 8 }); return arguments[0]; }
function few(a, b, c) { return arguments.length + ":" + arguments[2]; }
function strictFew(a, b) { "use strict"; b = 1; return arguments.length + ":" + arguments[1]; }
function rest(a, ...r) { a = 0; return arguments[0] + ":" + arguments.length; }
function dflt(a = 1) { a = 2; return arguments[0]; }
for (let i = 0; i < 3000; i++) {
  out.length = 0;
  out.push(g(1, 2), h(1), k(1), m(1), s(1), e(1), c(1), v(1), fd(1), fi(1), da(1),
    de(1), ar(6), w(1), inc(1), obj(1), few(1, 2), strictFew(1, 2), rest(5, 6), dflt(7));
}
console.log(out.join(","));
