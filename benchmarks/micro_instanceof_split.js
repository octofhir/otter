const b = Buffer.from('hello');
const o = {};
function run(label, v) {
  let n = 0;
  const t0 = process.hrtime.bigint();
  for (let i = 0; i < 5e6; i++) if (v instanceof Buffer) n++;
  console.log(label, n, Number(process.hrtime.bigint() - t0) / 5e6, 'ns');
}
run('buffer', b);
run('object', o);
run('u8', new Uint8Array(4));
