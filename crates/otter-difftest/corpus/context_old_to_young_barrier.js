// Old-to-young edges from captured bindings. Long-lived closures and the
// environments they capture are promoted through allocation churn; a hot
// loop then stores freshly allocated objects into those captured bindings
// from sibling closures and reads them back after more churn. Plain
// closures, a direct-eval `var` binding, a suspended generator's captured
// locals and a per-iteration binding kept by a promoted closure all take
// the same stores, so every captured-binding store path needs its barrier.
function churn(rounds) {
  let sink = 0;
  for (let k = 0; k < rounds; k++) {
    const junk = new Array(512).fill(k & 255);
    sink += junk[511];
  }
  return sink;
}

function makeHolder(id) {
  let slot = { n: -1, payload: ["init" + id] };
  let count = 0;
  const reader = () => slot;
  const inner = () => (v) => { slot = v; count++; };
  return { reader, writer: inner(), count: () => count };
}

function makeEvalHolder(id) {
  eval("var evalSlot = { n: -1, tag: 'eval-init" + id + "' }");
  return {
    reader: () => evalSlot,
    writer: (v) => { evalSlot = v; },
  };
}

function* parkedGenerator() {
  let parked = { n: -1, tag: "parked-init" };
  const reader = () => parked;
  const writer = (v) => { parked = v; };
  let command = yield { reader, writer };
  while (command !== "stop") {
    parked = { n: parked.n + 1000, tag: "resumed:" + command };
    command = yield parked;
  }
  return parked.tag;
}

function makeLoopKeeper() {
  const kept = [];
  for (let i = 0; i < 4; i++) {
    let box = { i, tag: "box" + i };
    kept.push({ read: () => box, write: (v) => { box = v; } });
  }
  return kept;
}

function exercise(n) {
  const holders = [];
  for (let i = 0; i < 32; i++) holders.push(makeHolder(i));
  const evalHolder = makeEvalHolder(7);
  const gen = parkedGenerator();
  const genTools = gen.next().value;
  const keepers = makeLoopKeeper();
  let sink = churn(3000);

  let checksum = 0;
  for (let i = 0; i < n; i++) {
    const h = holders[i & 31];
    h.writer({ n: i, payload: [i, "p" + i] });
    evalHolder.writer({ n: i * 2, tag: "e" + i });
    genTools.writer({ n: i * 3, tag: "g" + i });
    keepers[i & 3].write({ i: i * 4, tag: "k" + i });
    if (i % 250 === 0) sink += churn(40);
    checksum = (checksum + h.reader().n + evalHolder.reader().n + genTools.reader().n +
      keepers[i & 3].read().i) | 0;
  }
  sink += churn(3000);

  const lastHolder = holders.map(h => h.reader().payload[1]).slice(0, 4).join("/");
  const counts = holders.reduce((a, h) => a + h.count(), 0);
  const resumed = gen.next("r1").value.tag;
  genTools.writer({ n: 1, tag: "after-resume" });
  sink += churn(500);
  const afterResume = genTools.reader().tag;
  const finished = gen.next("stop");
  const keeperTags = keepers.map(k => k.read().tag).join("/");
  return [checksum, lastHolder, counts, evalHolder.reader().tag, resumed, afterResume,
    finished.value, keeperTags, sink > 0].join(",");
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("barrier " + exercise(3000));
  for (const line of lines) console.log(line);
  return "context_old_to_young_barrier:" + lines.length;
}
run();
