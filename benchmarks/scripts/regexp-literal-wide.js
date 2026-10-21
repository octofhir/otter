// A single UTF-16 needle with many false occurrences at odd byte offsets.
const subject = "Xā".repeat(256) + "Ā";
const re = /Ā/;
function wideLiteralKernel() {
  let count = 0;
  for (let i = 0; i < 100000; i++) if (re.test(subject)) count++;
  return count;
}
const warmups = [], samples = [];
for (let i = 0; i < 16; i++) {
  const start = performance.now();
  const result = wideLiteralKernel();
  const elapsed = performance.now() - start;
  if (result !== 100000) throw new Error("checksum " + result);
  (i < 6 ? warmups : samples).push(elapsed);
}
console.log(JSON.stringify({ checksum: 100000, warmupMs: warmups, samplesMs: samples }));
