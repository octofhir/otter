// Constructed receivers that never leave their compilation are not allocated:
// field reads, branch-merged field writes, prototype methods and deopts while
// the object exists only as its fields.
function Vec(x, y) {
  this.x = x;
  this.y = y;
}
Vec.prototype.dot = function (other) {
  return this.x * other.x + this.y * other.y;
};
Vec.prototype.scaled = function (k) {
  return new Vec(this.x * k, this.y * k);
};

function lengths(n) {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const v = new Vec(i, i + 1);
    sum += v.dot(v);
  }
  return sum;
}

function merged(n) {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const v = new Vec(i, 1);
    if (i % 3 === 0) {
      v.x = -i;
    } else {
      v.y = i * 2;
    }
    sum += v.x + v.y;
  }
  return sum;
}

function chained(n) {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    sum += new Vec(i, 2).scaled(3).dot(new Vec(1, 1));
  }
  return sum;
}

// A rarely taken branch lets the receiver escape.
const kept = [];
function escapesRarely(n) {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const v = new Vec(i, -i);
    if (i % 997 === 0) kept.push(v);
    sum += v.x - v.y;
  }
  return sum;
}

// The deopt happens while `v` is only its fields; the frame after the deopt
// reads the object it materialized.
function deoptsWhileVirtual(n, poison) {
  let out = 0;
  for (let i = 0; i < n; i++) {
    const v = new Vec(i, i * 2);
    const add = i === poison ? "s" : 1;
    out += add;
    out = typeof out === "string" ? out.length + v.x + v.y : out + v.x - v.y;
  }
  return out;
}

const results = [];
for (let round = 0; round < 40; round++) {
  results.push(lengths(200), merged(200), chained(200), escapesRarely(2000));
}
results.push(deoptsWhileVirtual(20000, -1), deoptsWhileVirtual(20000, 15000));
JSON.stringify({
  results: results.slice(-8),
  kept: kept.length,
  first: kept[0].x + kept[0].y,
  last: kept[kept.length - 1].x,
});
