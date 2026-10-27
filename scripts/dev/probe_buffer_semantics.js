// Buffer.from(string) / Buffer.concat results match node.
const out = [];
const a = Buffer.from('héllo wörld');
out.push(a.length, a.toString(), a instanceof Buffer, a.constructor.name, Object.getPrototypeOf(a) === Buffer.prototype);
out.push(Buffer.from('68656c6c6f', 'hex').toString(), Buffer.from('aGk=', 'base64').toString());
const c = Buffer.concat([Buffer.from('ab'), new Uint8Array([99, 100]), Buffer.from('e')]);
out.push(c.toString(), c.length, c instanceof Buffer, c.buffer.byteLength >= c.length);
out.push(Buffer.concat([Buffer.from('abc'), Buffer.from('def')], 4).toString());
out.push(Buffer.concat([Buffer.from('ab')], 5).toString('hex'));
out.push(Buffer.concat([]).length, Buffer.from('').length);
try { Buffer.concat([1]); } catch (e) { out.push(e.code); }
const s = new Uint8Array(new ArrayBuffer(8), 2, 3); s.set([65, 66, 67]);
out.push(Buffer.concat([s]).toString());
class A { static s() { return 1; } } class B extends A { static t() { return 2; } }
out.push(B.s(), B.t(), A.name, B.name, A.length, Object.getPrototypeOf(B) === A);
Object.setPrototypeOf(B, { s() { return 9; } }); out.push(B.s());
console.log(JSON.stringify(out));
