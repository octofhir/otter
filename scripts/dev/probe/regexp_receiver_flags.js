// Which RegExp receivers miss the generated `exec` lookup: a hot `Exec`
// helper, then each kind of RegExp use the Octane regexp fixture makes.
function Exec(re, string) {
  re.lastIndex = 0;
  var array = re.exec(string);
  return array ? array.length : 0;
}
const plain = /^ba/;
let sum = 0;
for (let i = 0; i < 200000; i++) sum += Exec(plain, 'pyvpx' + (i & 7));
const kind = process.argv[2] || 'none';
const other = /a(b)?/g;
if (kind === 'replace') 'abab'.replace(other, '');
if (kind === 'split') 'abab'.split(other);
if (kind === 'match') 'abab'.match(other);
if (kind === 'test') other.test('abab');
if (kind === 'search') 'abab'.search(other);
for (let i = 0; i < 200000; i++) sum += Exec(i & 1 ? other : plain, 'xabx' + (i & 7));
console.log(kind, sum);
