// Array.prototype.splice over dense arrays: growth, shrink, numeric storage
// taking objects, holes, frozen and accessor cases, and a token-insertion loop.
var out = [];
function show(a) { return JSON.stringify(a); }
var a = [1, 2, 3, 4, 5];
out.push(show(a.splice(1, 2)), show(a));
a = [1, 2, 3, 4, 5]; out.push(show(a.splice(2, 0, 'x', 'y', 'z')), show(a));
a = [1, 2, 3]; out.push(show(a.splice(-1, 5, { o: 1 })), show(a));
a = [1.5, 2.5, 3.5]; out.push(show(a.splice(1, 1, 'str', null)), show(a), typeof a[1]);
a = [1, , 3]; out.push(show(a.splice(0, 1, 9)), show(a), 1 in a);
a = Object.freeze([1, 2]); try { a.splice(0, 1); } catch (e) { out.push(e.constructor.name); }
a = [1, 2, 3]; Object.defineProperty(a, 'length', { writable: false });
try { a.splice(1, 0, 4); } catch (e) { out.push(e.constructor.name); }
out.push(show(a));
a = [1, 2, 3, 4]; out.push(show(a.splice()), show(a.splice(2)), show(a));
a = []; for (var i = 0; i < 2000; i++) a.splice(a.length >> 1, 0, { i: i });
var s = 0; for (var i = 0; i < a.length; i++) s += a[i].i * (i & 7);
for (var i = 0; i < 1000; i++) a.splice(i, 1);
out.push(a.length, s, a[0].i, a[a.length - 1].i);
// Hot appends after a splice extend the length the splice published.
function grow(list, n) { for (var i = 0; i < n; i++) list.push({ g: i }); }
var tokens = [];
for (var round = 0; round < 300; round++) {
  grow(tokens, 20);
  tokens.splice(tokens.length >> 1, round & 1, { s: round });
  if (round % 3 === 0) tokens.splice(0, 2);
}
var junk = []; for (var i = 0; i < 50000; i++) junk.push({ j: i });
var g = 0; for (var i = 0; i < tokens.length; i++) g += (tokens[i].g | 0) + (tokens[i].s | 0);
out.push(tokens.length, g, JSON.stringify(tokens[tokens.length - 1]));
class Sub extends Array {}
var b = Sub.from([1, 2, 3]); var r = b.splice(0, 1); out.push(r instanceof Sub, show(b));
Array.prototype[7] = 'proto'; a = [1, 2, 3]; a.splice(1, 1); out.push(show(a), a.length); delete Array.prototype[7];
console.log(out.join('\n'));
