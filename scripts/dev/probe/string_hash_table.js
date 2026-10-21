// The TypeScript compiler's StringHashTable: an object keyed by identifier
// spellings that arrive as fresh substrings of the source text.
function BlockIntrinsics() {
  this.prototype = undefined;
  this.toString = undefined;
  this.toLocaleString = undefined;
  this.valueOf = undefined;
  this.hasOwnProperty = undefined;
  this.propertyIsEnumerable = undefined;
  this.isPrototypeOf = undefined;
  this["constructor"] = undefined;
}
function StringHashTable() {
  this.itemCount = 0;
  this.table = new BlockIntrinsics();
}
StringHashTable.prototype.add = function (key, data) {
  if (this.table[key] != undefined) return false;
  this.table[key] = data;
  this.itemCount++;
  return true;
};
StringHashTable.prototype.lookup = function (key) {
  var data = this.table[key];
  if (data != undefined) return data;
  return null;
};
var words = [];
for (var i = 0; i < 400; i++) words.push("ident" + (i * 7919 % 1000) + "x");
var text = words.join(" ");
var spans = [];
for (var p = 0, i = 0; i < words.length; i++) { spans.push([p, p + words[i].length]); p += words[i].length + 1; }
var t0 = Date.now(), hits = 0;
for (var round = 0; round < 3000; round++) {
  var tables = [new StringHashTable(), new StringHashTable(), new StringHashTable()];
  for (var i = 0; i < spans.length; i++) {
    var key = text.substring(spans[i][0], spans[i][1]);
    var table = tables[i % 3];
    if (table.lookup(key) === null) table.add(key, i + 1); else hits++;
    if (tables[(i + 1) % 3].lookup(key) !== null) hits++;
  }
}
console.log(hits, Date.now() - t0, "ms");
