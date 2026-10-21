// Fixed work over literals used by the Octane RegExp workload. A process
// reports cold first invocation, warmups and steady execution separately.
const inputs = ["x=NQ_VQ d1 d2", "nothing here", "NQ_VQ=d1=d2", "éĀΩ=", "d1NQ_VQd2"];
const patterns = [/=/, /NQ_VQ/, /d1/, /d2/, /é/, /ĀΩ/];
function literalKernel() {
  let count = 0;
  for (let i = 0; i < 300000; i++) {
    if (patterns[i % patterns.length].test(inputs[i % inputs.length])) count++;
  }
  return count;
}
const expected = 140000;
const warmups = [], samples = [];
for (let i = 0; i < 16; i++) {
  const start = performance.now();
  const result = literalKernel();
  const elapsed = performance.now() - start;
  if (result !== expected) throw new Error("checksum " + result);
  (i < 6 ? warmups : samples).push(elapsed);
}
console.log(JSON.stringify({ checksum: expected, warmupMs: warmups, samplesMs: samples }));
