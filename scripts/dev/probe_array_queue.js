// Arrays used as queues and stacks: shift/unshift/splice block moves over
// tagged, packed and holey numeric storage, with holes and GC pressure.
var out = [];
var q = []; for (var i = 0; i < 3000; i++) q.push({ i: i });
var s = 0; while (q.length > 1000) { var x = q.shift(); s += x.i; if (x.i % 7 === 0) q.push({ i: -x.i }); }
out.push(q.length, s, q[0].i, q[q.length - 1].i);
var n = []; for (var i = 0; i < 500; i++) n.push(i * 0.5); for (var i = 0; i < 200; i++) n.shift(); out.push(n.length, n[0], n[299]);
var h = [1.5, , 3.5, , 5.5]; out.push(h.shift(), h.length, 0 in h, 1 in h, h[1]); h.unshift(9, undefined, 8); out.push(JSON.stringify(h), 1 in h);
var t = [{ a: 1 }, , { b: 2 }]; out.push(JSON.stringify(t.shift()), 0 in t, t.length);
var u = []; for (var i = 0; i < 300; i++) u.unshift({ u: i }); out.push(u.length, u[0].u, u[299].u);
var junk = []; for (var i = 0; i < 40000; i++) junk.push({ j: i });
var m = 0; for (var i = 0; i < u.length; i++) m += u[i].u; out.push(m);
var p = [1, 2, 3, 4, 5, 6, 7, 8]; p.splice(2, 3); p.splice(1, 0, 'a', 'b', 'c', 'd'); out.push(JSON.stringify(p));
console.log(out.join('\n'));
