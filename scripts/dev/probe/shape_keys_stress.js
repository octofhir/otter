// Wide and branching hidden-class lineages: shared key tables, in-place
// appends, prefix copies on branches, string- and atom-keyed lookups.
function build(n, tag) {
  var o = {};
  for (var i = 0; i < n; i++) o["k" + i] = i;
  o["tag" + tag] = tag;
  return o;
}
var objs = [];
for (var r = 0; r < 200; r++) objs.push(build(10 + (r % 30), r % 7));
var sum = 0;
for (var r = 0; r < objs.length; r++) {
  var o = objs[r];
  for (var i = 0; i < 45; i++) {
    var v = o["k" + i];
    if (v !== undefined) sum += v;
  }
  sum += o["tag" + (r % 7)] | 0;
  sum += Object.keys(o).length;
  sum += Object.getOwnPropertyDescriptor(o, "k5") ? 1 : 0;
  sum += ("k9" in o) ? 1 : 0;
}
function F() {}
for (var i = 0; i < 40; i++) F["s" + i] = i;
for (var i = 0; i < 40; i++) sum += F["s" + i];
Object.defineProperty(objs[3], "k2", { enumerable: false });
sum += Object.keys(objs[3]).length;
delete objs[4].k3;
sum += objs[4].k3 === undefined ? 1 : 0;
console.log(sum);
