// A loop entered through OSR at a versioned preheader: the preheader's block
// parameter (`next`) starts as a copy of a value that stays live through the
// loop (`queue`). On the ordinary entry both hold the same value, so the
// preheader's in-edge move copies one into the other; on the OSR path they
// differ, and an entry that ran that move returned the walked tail instead of
// the list head.
function Node(id) { this.id = id; this.link = null; }
Node.prototype.addTo = function (queue) {
  this.link = null;
  if (queue == null) return this;
  var peek, next = queue;
  while ((peek = next.link) != null) next = peek;
  next.link = this;
  return queue;
};

let bad = 0;
let first = "";
let acc = 0;
let queue = null;
for (let i = 0; i < 4000; i++) {
  queue = new Node(i).addTo(queue);
  if ((i & 15) === 15) {
    if (queue.id !== i - 15) {
      bad++;
      if (first === "") first = i + " => " + queue.id;
    }
    acc += queue.id;
    queue = null;
  }
}
console.log("osr_preheader_parameters", bad, first, acc);
