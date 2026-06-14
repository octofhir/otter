// JSON stringify + parse round-trip — serializer/parser throughput.
function makeRecord(i) {
  return {
    id: i,
    name: "user_" + i,
    active: (i & 1) === 0,
    score: i * 1.5,
    tags: ["a", "b", "c", "tag" + (i % 100)],
    meta: { created: i * 1000, nested: { a: i, b: -i, c: "x".repeat(i % 16) } },
  };
}
const data = [];
for (let i = 0; i < 5000; i++) data.push(makeRecord(i));

let acc = 0;
for (let r = 0; r < 40; r++) {
  const s = JSON.stringify(data);
  const back = JSON.parse(s);
  acc += back.length + s.length;
}
console.log(acc);
