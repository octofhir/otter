// Method calls on RegExp and primitive string receivers, whose methods live
// on the realm's %RegExp.prototype% and %String.prototype%: hot sites first
// see only canonical receivers, then an own `exec`, a subclass, a changed
// [[Prototype]], a redefined prototype method and a deleted one.
const results = [];

function execAll(re, text) {
  let count = 0;
  let sum = 0;
  re.lastIndex = 0;
  let match;
  while ((match = re.exec(text)) !== null) {
    count++;
    sum += match.index + match[0].length;
    if (!re.global) break;
  }
  return count * 1000 + sum;
}

function testMany(re, words) {
  let hits = 0;
  for (let i = 0; i < words.length; i++) if (re.test(words[i])) hits++;
  return hits;
}

function rewrite(text, i) {
  return text.replace(/o/g, '0').split(' ').join('_') + text.slice(i % 5).toUpperCase().length;
}

const text = 'the quick brown fox jumps over the lazy dog again and again';
const words = text.split(' ');
let canonical = 0;
for (let i = 0; i < 4000; i++) {
  canonical = (canonical + execAll(/[aeiou]+/g, text) + testMany(/^[a-m]/, words)) % 1000003;
  canonical = (canonical + rewrite(text, i).length) % 1000003;
}
results.push(canonical);

// An own `exec` on one instance: the expando shadows the prototype.
const own = /o/g;
own.exec = function () { return null; };
results.push(execAll(own, text), execAll(/o/g, text));

// A subclass instance resolves `exec` on the subclass prototype.
class Tracing extends RegExp {
  exec(s) {
    const m = super.exec(s);
    if (m) m.traced = true;
    return m;
  }
}
const sub = new Tracing('a', 'g');
let traced = 0;
let m;
while ((m = sub.exec(text)) !== null) if (m.traced) traced++;
results.push(traced);

// A changed [[Prototype]].
const moved = /e/g;
Object.setPrototypeOf(moved, { exec() { return null; }, test() { return 'proto'; }, lastIndex: 0 });
results.push(execAll(moved, text), moved.test('e'));

// Redefine, then delete, a prototype method while the sites are hot.
const originalReplace = String.prototype.replace;
let redefined = 0;
for (let i = 0; i < 3000; i++) {
  if (i === 1500) String.prototype.replace = function () { return 'x'; };
  redefined += rewrite(text, i).length;
}
String.prototype.replace = originalReplace;
results.push(redefined);

const originalTest = RegExp.prototype.test;
let afterDelete = 0;
for (let i = 0; i < 3000; i++) {
  if (i === 2000) delete RegExp.prototype.test;
  try {
    afterDelete += testMany(/^t/, words);
  } catch (e) {
    afterDelete += 100000;
    break;
  }
}
RegExp.prototype.test = originalTest;
results.push(afterDelete, testMany(/^t/, words));

console.log(JSON.stringify(results));
