// Ordinary field bounds retain live values, nullable arms and alias effects.
const results = [];
class Child { constructor(x, y) { this.x = x; this.y = y; } }
class Other { constructor(x, y) { this.y = y; this.x = x; } }
class Third { constructor(x, y) { this.extra = 0; this.x = x; this.y = y; } }
const first = new Child(3, 5);
const second = new Other(7, 11);
const third = new Third(13, 17);

class StableHolder { constructor(child) { this.child = child; } }
function readStable(holder) { const n = holder.child; return n.x + n.y; }
const stable = new StableHolder(first);
const cold = new StableHolder(first);
for (let i = 0; i < 6000; i++) readStable(stable);
results.push(readStable(stable));
cold.child = second;
results.push(readStable(cold), readStable(stable));
cold.child = third;
results.push(readStable(cold));
cold.child = 41;
results.push(cold.child, Number.isNaN(readStable(cold)));

class NullableHolder { constructor(child) { this.child = child; } }
function readNullable(holder) { const n = holder.child; return n != null ? n.x + n.y : -1; }
function readStrict(holder) { const n = holder.child; return n !== null ? n.x + n.y : -2; }
function readTruthy(holder) { const n = holder.child; return n ? n.x + n.y : -3; }
const nullable = new NullableHolder(first);
const nullCold = new NullableHolder(null);
const undefinedCold = new NullableHolder(undefined);
for (let i = 0; i < 6000; i++) {
  readNullable(i % 3 === 0 ? nullCold : i % 3 === 1 ? undefinedCold : nullable);
  readStrict(i % 2 === 0 ? nullCold : nullable);
  readTruthy(i % 3 === 0 ? nullCold : i % 3 === 1 ? undefinedCold : nullable);
}
results.push(readNullable(nullCold), readNullable(undefinedCold), readNullable(nullable));
results.push(readStrict(nullCold));
let strictThrows = false;
try { readStrict(undefinedCold); } catch (e) { strictThrows = e instanceof TypeError; }
results.push(strictThrows, readTruthy(nullCold), readTruthy(undefinedCold), readTruthy(nullable));

class AliasChild { constructor(x, y) { this.x = x; this.y = y; } }
class AliasHolder { constructor(child) { this.child = child; } }
const aliasChild = new AliasChild(19, 23);
const alias = new AliasHolder(aliasChild);
let effects = 0;
function effect(n, mode) {
  effects++;
  if (mode === 1) n.extra = 1;
  if (mode === 2) delete n.y;
  if (mode === 3) Object.defineProperty(n, 'x', { get() { effects++; return 31; }, configurable: true });
  if (mode === 4) Object.setPrototypeOf(n, { y: 37 });
}
function readAlias(holder, mode) { const n = holder.child; effect(n, mode); return n.x + n.y; }
for (let i = 0; i < 6000; i++) readAlias(alias, 0);
effects = 0;
results.push(readAlias(alias, 1), effects);
effects = 0;
results.push(Number.isNaN(readAlias(alias, 2)), effects);
effects = 0;
results.push(Number.isNaN(readAlias(alias, 3)), effects);
effects = 0;
results.push(readAlias(alias, 4), effects);

class SwapHolder { constructor(child) { this.child = child; } }
const swap = new SwapHolder(new Child(43, 47));
const replacement = new Child(53, 59);
function replace(holder, child) { holder.child = child; }
function compareLoaded(holder, child) {
  const before = holder.child;
  replace(holder, child);
  const after = holder.child;
  return [before === after, before.x + after.y];
}
for (let i = 0; i < 6000; i++) compareLoaded(swap, i % 2 === 0 ? first : replacement);
swap.child = new Child(43, 47);
const compared = compareLoaded(swap, replacement);
results.push(compared[0], compared[1]);

class WideHolder {
  constructor(child) {
    for (let i = 0; i < 75; i++) this['pad' + i] = i;
    this.child = child;
  }
}
function readWide(holder) { const n = holder.child; return n != null ? n.x + n.y : -4; }
const wide = new WideHolder(first);
for (let i = 0; i < 6000; i++) readWide(wide);
results.push(readWide(wide));
wide.child = second;
results.push(readWide(wide));

class MultiA { constructor(child) { this.child = child; } }
class MultiB { constructor(child) { this.child = child; this.extra = 1; } }
function setMulti(holder, child) { holder.child = child; }
function readMulti(holder) { const n = holder.child; return n.x + n.y; }
const multiA = new MultiA(first);
const multiB = new MultiB(first);
for (let i = 0; i < 6000; i++) {
  const h = i % 2 === 0 ? multiA : multiB;
  setMulti(h, first);
  readMulti(h);
}
setMulti(multiB, second);
results.push(readMulti(multiB), readMulti(multiA));

function makeLiteral(child) { return { literalChild: child }; }
function readLiteral(holder) { const n = holder.literalChild; return n.x + n.y; }
let literal = makeLiteral(first);
for (let i = 0; i < 6000; i++) {
  literal = makeLiteral(first);
  readLiteral(literal);
}
const literalCold = makeLiteral(second);
results.push(readLiteral(literalCold), readLiteral(literal));

class BeforeAssign {
  constructor(child) { this.seen = this.child; this.child = child; }
}
let before;
for (let i = 0; i < 6000; i++) before = new BeforeAssign(first);
results.push(before.seen === undefined, before.child.x);

class Link { constructor(value, next) { this.value = value; this.next = next; } }
function walk(head) {
  let sum = 0;
  for (let n = head; n != null; n = n.next) sum += n.value;
  return sum;
}
const chain = new Link(1, new Link(2, new Link(3, null)));
for (let i = 0; i < 6000; i++) walk(chain);
const alternateTail = { value: 5, next: null, extra: true };
chain.next.next = alternateTail;
results.push(walk(chain));
const longChain = new Link(1, null);
let tail = longChain;
for (let i = 0; i < 3000; i++) { tail.next = new Link(1, null); tail = tail.next; }
results.push(walk(longChain));

class LoopChild { constructor() { this.x = 1; this.y = 2; } }
class LoopHolder { constructor(child, other) { this.child = child; this.other = other; } }
let loopEffects = 0;
function loopAlias(holder, mutate) {
  const pending = holder.child;
  const other = holder.other;
  let total = other.y;
  for (let i = 0; i < 3; i++) {
    total += pending.y;
    if (mutate && i === 0) {
      loopEffects++;
      Object.defineProperty(pending, 'y', { get() { loopEffects++; return 9; } });
    }
  }
  return total;
}
const loopHolder = new LoopHolder(new LoopChild(), new LoopChild());
for (let i = 0; i < 6000; i++) loopAlias(loopHolder, false);
results.push(loopAlias(loopHolder, true), loopEffects);

class SelfHolder {}
function appendSelf(holder) { holder.self = holder; }
function readSelf(holder) { const n = holder.self; return n.self === holder; }
let self;
for (let i = 0; i < 6000; i++) {
  self = new SelfHolder();
  appendSelf(self);
  readSelf(self);
}
results.push(readSelf(self), self.self === self);

class DemandHolder { constructor(child) { this.child = child; } }
function returnField(holder) { return holder.child; }
let demandEffects = 0;
function discardThenUse(unused, used) {
  demandEffects++;
  const ignored = unused.child;
  const selected = used.child;
  return selected.x + selected.y;
}
const demandUnused = new DemandHolder(first);
const demandUsed = new DemandHolder(first);
for (let i = 0; i < 6000; i++) {
  returnField(demandUnused);
  discardThenUse(demandUnused, demandUsed);
}
demandUnused.child = 67;
demandEffects = 0;
results.push(returnField(demandUnused), discardThenUse(demandUnused, demandUsed), demandEffects);

class PhiChild { constructor(x) { this.x = x; this.y = x + 1; } }
class PhiOther { constructor(x) { this.y = x + 1; this.x = x; } }
class PhiHolder { constructor(child) { this.child = child; } }
const phiFirst = new PhiChild(31);
const phiSecond = new PhiOther(41);
const phiSource = new PhiHolder(phiSecond);
const phiDestination = new PhiHolder(phiFirst);
function pickCurrent(current, holder, flag) {
  const ignored = current.x;
  const loaded = holder.child;
  return flag ? current : loaded;
}
function assignPicked(destination, current, holder, flag) {
  destination.child = pickCurrent(current, holder, flag);
  return destination.child.x + destination.child.y;
}
let phiEffects = 0;
function usePicked(current, holder, flag, mutate) {
  const selected = pickCurrent(current, holder, flag);
  if (mutate) {
    phiEffects++;
    Object.defineProperty(selected, 'x', { get() { phiEffects++; return 101; } });
  }
  return selected.x + selected.y;
}
for (let i = 0; i < 6000; i++) {
  assignPicked(phiDestination, phiFirst, phiSource, i % 2 === 0);
  usePicked(phiFirst, phiSource, i % 2 === 0, false);
}
results.push(assignPicked(phiDestination, phiFirst, phiSource, true));
results.push(assignPicked(phiDestination, phiFirst, phiSource, false));
phiEffects = 0;
results.push(usePicked(phiFirst, phiSource, true, true), phiEffects);
phiSource.child = third;
results.push(assignPicked(phiDestination, phiFirst, phiSource, false));

let freshEffects = 0;
function lateFreshValue(value) { freshEffects++; return value; }
function appendFreshNullish(value) {
  const holder = {};
  holder.freshNullish = lateFreshValue(value);
  return holder;
}
function readFreshNullish(holder) {
  const value = holder.freshNullish;
  return value == null ? -5 : value.x + value.y;
}
function appendFreshChild(value) {
  const holder = {};
  holder.freshChild = lateFreshValue(value);
  return holder;
}
function readFreshChild(holder) {
  const value = holder.freshChild;
  return value == null ? -6 : value.x + value.y;
}
function appendFreshSelf() {
  const holder = {};
  holder.freshSelf = holder;
  return holder;
}
for (let i = 0; i < 6000; i++) {
  readFreshNullish(appendFreshNullish(i % 2 === 0 ? null : undefined));
  readFreshChild(appendFreshChild(first));
  const holder = appendFreshSelf();
  if (holder.freshSelf !== holder) throw new Error('fresh self identity');
}
freshEffects = 0;
const freshNull = appendFreshNullish(null);
const freshUndefined = appendFreshNullish(undefined);
const freshPrimitive = appendFreshNullish(73);
const freshChild = appendFreshChild(second);
const freshChildNull = appendFreshChild(null);
const freshChildUndefined = appendFreshChild(undefined);
const freshChildPrimitive = appendFreshChild(79);
const freshSelf = appendFreshSelf();
results.push(readFreshNullish(freshNull), readFreshNullish(freshUndefined));
results.push(freshPrimitive.freshNullish, Number.isNaN(readFreshNullish(freshPrimitive)));
results.push(readFreshChild(freshChild), readFreshChild(freshChildNull), readFreshChild(freshChildUndefined));
results.push(freshChildPrimitive.freshChild, Number.isNaN(readFreshChild(freshChildPrimitive)));
results.push(freshSelf.freshSelf === freshSelf, freshEffects);

class Member0 { constructor(value) { this.member = value; } }
class Member1 { constructor(value) { this.pad = 1; this.member = value; } }
class Member2 { constructor(value) { this.pad = 1; this.extra = 2; this.member = value; } }
const members = [new Member0(first), new Member1(first), new Member2(first)];
function storeNamedMember(holder, value) { holder.member = value; }
function readMember(holder) {
  const value = holder.member;
  return value == null ? -11 : value.x + value.y;
}
for (let i = 0; i < 6000; i++) {
  const holder = members[i % 3];
  storeNamedMember(holder, i % 5 === 0 ? null : i % 7 === 0 ? undefined : first);
  readMember(holder);
}
for (const holder of members) {
  storeNamedMember(holder, second);
  results.push(readMember(holder));
  storeNamedMember(holder, third);
  results.push(readMember(holder));
  storeNamedMember(holder, 83);
  results.push(holder.member, Number.isNaN(readMember(holder)));
}

function makeMembershipLiteral(value) { return { member: value, constant: 1 }; }
function readMembershipLiteral(holder) {
  const value = holder.member;
  return value == null ? -12 : value.x + value.y + holder.constant;
}
for (let i = 0; i < 6000; i++) readMembershipLiteral(makeMembershipLiteral(first));
for (const value of [second, undefined, null, third]) {
  const holder = makeMembershipLiteral(value);
  results.push(holder.member === value, readMembershipLiteral(holder));
}
const primitiveLiteral = makeMembershipLiteral(89);
results.push(primitiveLiteral.member, Number.isNaN(readMembershipLiteral(primitiveLiteral)));

console.log(JSON.stringify(results));
