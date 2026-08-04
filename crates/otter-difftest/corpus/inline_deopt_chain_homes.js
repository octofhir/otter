// A three-frame inline chain deoptimizes inside its innermost body. One
// register dump rebuilds every frame at once, so a caller's value and a
// callee's value must never share a machine home: here the middle frame's
// receiver stays live across the spliced inner call, whose own receiver load
// is the natural next tenant of that register.

function Bag(items) {
  this.elms = items;
}

Bag.prototype.at = function (index) {
  const value = this.elms[index];
  return value + 1;
};

function Holder(bag) {
  this.bag = bag;
}

Holder.prototype.itemAt = function (index) {
  return this.bag.at(index);
};

function drive(holder, count) {
  let last = 0;
  for (let i = 0; i < count; i++) {
    last = holder.itemAt(i & 3);
  }
  return last;
}

const items = [1, 2, 3, 4];
const holder = new Holder(new Bag(items));

const warm = drive(holder, 40000);
// The innermost addition now leaves int32 while both callers are paused.
items[3] = 2147483647;
const overflowed = drive(holder, 40000);

JSON.stringify({ warm, overflowed });
