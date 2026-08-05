// A spliced method body deoptimizes while its caller is running as a
// generated direct-call target, so the exit owns no interpreter frame to
// rebuild the chain on top of. Every frame of the chain must come back at its
// exact PC and produce the interpreter's answer.

function Cell(value) {
  this.value = value;
}

Cell.prototype.step = function (n) {
  return this.value + n;
};

// Spliced callee of `drive`, and itself the frame the chain is rebuilt from.
function run(cell, n) {
  return cell.step(n) * 2;
}

function drive(cell, iterations) {
  let total = 0;
  for (let i = 0; i < iterations; i++) {
    total += run(cell, i & 3);
  }
  return total;
}

const cell = new Cell(1);
let total = drive(cell, 60000);

// The spliced body's numeric speculation stops holding, mid-chain.
cell.value = "s";
const tail = drive(cell, 8);

JSON.stringify({ total, tail });
