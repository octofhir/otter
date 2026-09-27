// Six nested function levels interleaved with block scopes. Levels 3 and 4
// reference no outer binding themselves, so the scope chain must still pass
// through them. A level-5 write to a level-1 binding is seen by a level-2
// sibling; a level-3 shadow of a level-1 name is what level 6 reads; level 4
// owns a per-iteration `let`; level 1 has more than 16 captured bindings in
// one scope. The whole chain runs hot.
function level1(seed) {
  let shared = { n: seed };
  let name = "L1";
  let s0 = { v: 0 }, s1 = { v: 1 }, s2 = { v: 2 }, s3 = { v: 3 }, s4 = { v: 4 };
  let s5 = { v: 5 }, s6 = { v: 6 }, s7 = { v: 7 }, s8 = { v: 8 }, s9 = { v: 9 };
  let s10 = { v: 10 }, s11 = { v: 11 }, s12 = { v: 12 }, s13 = { v: 13 }, s14 = { v: 14 };
  let s15 = { v: 15 }, s16 = { v: 16 }, s17 = { v: 17 }, s18 = { v: 18 }, s19 = { v: 19 };
  const sibling = function level2Sibling() { return shared.n + ":" + name; };
  const bumpAll = () => {
    s0 = { v: s0.v + 1 };
    s19 = { v: s19.v + 1 };
    s10 = { v: s10.v * 2 };
  };
  function level2(k) {
    let result;
    {
      let block2 = { b: k };
      const readBlock2 = () => block2.b;
      function level3() {
        let name = "L3";
        {
          let unused3 = { u: 3 };
          return function level4() {
            const fns = [];
            for (let i = 0; i < 3; i++) {
              let own4 = { i };
              fns.push(function level5(delta) {
                shared = { n: shared.n + delta };
                {
                  let block5 = { j: i * 100 };
                  return function level6() {
                    const wide = s0.v + s1.v + s2.v + s3.v + s4.v + s5.v + s6.v + s7.v + s8.v + s9.v +
                      s10.v + s11.v + s12.v + s13.v + s14.v + s15.v + s16.v + s17.v + s18.v + s19.v;
                    return name + "/" + block2.b + "/" + own4.i + "/" + block5.j + "/" + wide + "/" + shared.n;
                  };
                }
              });
              own4 = { i: i + 10 };
            }
            return fns;
          };
        }
      }
      result = { level4: level3(), readBlock2 };
      block2 = { b: k + 1 };
    }
    return result;
  }
  return { level2, sibling, bumpAll };
}

function structure() {
  const root = level1(5);
  const { level4, readBlock2 } = root.level2(7);
  const fns = level4();
  const before = root.sibling();
  const inner = fns.map((f, idx) => f(idx + 1)());
  const after = root.sibling();
  root.bumpAll();
  const rebumped = fns[0](0)();
  return [before, inner.join(","), after, rebumped, readBlock2()].join("|");
}

function hot(n) {
  const root = level1(0);
  const { level4 } = root.level2(1);
  let checksum = 0;
  let last = "";
  for (let i = 0; i < n; i++) {
    const fns = level4();
    const f6 = fns[i % 3](1);
    last = f6();
    checksum = (checksum + last.length + (i & 7)) | 0;
    if ((i & 511) === 0) root.bumpAll();
  }
  return checksum + "|" + last + "|" + root.sibling();
}

function independentRoots(n) {
  const roots = [];
  for (let i = 0; i < n; i++) roots.push(level1(i).level2(i).level4()[i % 3]);
  let sum = 0;
  for (let i = 0; i < n; i++) sum += roots[i](i).call(null).length;
  return sum + "|" + roots[n - 1](0)();
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("structure " + structure());
  lines.push("hot " + hot(3000));
  lines.push("roots " + independentRoots(400));
  for (const line of lines) console.log(line);
  return "context_deep_nesting:" + lines.length;
}
run();
