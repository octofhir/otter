// Young array literals stored into aged holders: their element slabs promote
// while a root already copied an element young. Run on a `checked` build with
// OTTER_GC_VERIFY=1: the remembered-set check must report nothing.
// usage: probe_slab_promotion.js push|index|literal
var mode = process.argv[2];
var holders = [];
for (var i = 0; i < 2000; i++) holders.push({ decls: [ { k: i } ] });
var junk; for (var r = 0; r < 60; r++) { junk = []; for (var j = 0; j < 20000; j++) junk.push({ j: j }); }
function appendPush(h, v) { h.decls.push(v); }
function appendIndex(h, v) { h.decls[h.decls.length] = v; }
function appendLiteral(h, v) { h.decls = h.decls === undefined ? [v] : [v, h.decls[0]]; }
var f = mode === 'push' ? appendPush : mode === 'index' ? appendIndex : appendLiteral;
var s = 0;
for (var round = 0; round < 40; round++) {
  for (var i = 0; i < holders.length; i++) f(holders[i], { round: round, i: i });
  junk = []; for (var j = 0; j < 30000; j++) junk.push({ j: j });
  for (var i = 0; i < holders.length; i++) { var d = holders[i].decls; s += d[d.length - 1].round | 0; }
}
console.log(s);
