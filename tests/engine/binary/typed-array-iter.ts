/* otter-test:
name = "binary: TypedArray keys / values / entries snapshot"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}

// Foundation models keys/values/entries as snapshot arrays. Spec
// returns live iterators; the foundation upgrade is filed alongside
// the wider iterator-protocol work.
const a = new Uint8Array([10, 20, 30]);
const ks = a.keys();
if (ks[0] !== 0 || ks[2] !== 2) fail();
const vs = a.values();
if (vs[0] !== 10 || vs[2] !== 30) fail();
const es = a.entries();
if (es[0][0] !== 0 || es[0][1] !== 10) fail();
if (es[2][1] !== 30) fail();
