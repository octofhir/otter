// Step 8 widening: loop proof motion must preserve external/OSR entry,
// invalidation, resizable-buffer, accessor, reentry, and moving-GC semantics.

function packedNested(input, output, rounds) {
  let checksum = 0;
  for (let outer = 0; outer < rounds; outer++) {
    for (let index = 1; index < 5; index++) {
      const value = (input[index - 1] + input[index] + input[index + 1]) / 3;
      output[index] = value;
      checksum += value;
    }
  }
  return checksum;
}

const packedWarmInput = [0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5];
const packedWarmOutput = [0.25, 0.25, 0.25, 0.25, 0.25, 0.25, 0.25];
for (let warm = 0; warm < 5000; warm++) {
  packedNested(packedWarmInput, packedWarmOutput, warm < 8 ? 1 : 0);
}
const packedInput = [10.5, 20.5, 30.5, 40.5, 50.5, 60.5, 70.5];
const packedOutput = [0.75, 0.75, 0.75, 0.75, 0.75, 0.75, 0.75];
const packedChecksum = packedNested(packedInput, packedOutput, 40);

function invariantProperty(receiver, rounds) {
  let checksum = 0;
  for (let index = 0; index < rounds; index++) checksum += receiver.value;
  return checksum;
}

const propertyWarm = { value: 2 };
for (let warm = 0; warm < 5000; warm++) invariantProperty(propertyWarm, warm < 8 ? 4 : 0);
const propertyChanged = { padding: 1, value: 7 };
const changedShape = invariantProperty(propertyChanged, 64);

let accessorReads = 0;
function readAcrossInstallation(receiver, rounds) {
  let checksum = 0;
  for (let index = 0; index < rounds; index++) {
    if (index === 31) {
      Object.defineProperty(receiver, "value", {
        configurable: true,
        get() {
          accessorReads++;
          return 9;
        },
      });
    }
    checksum += receiver.value;
  }
  return checksum;
}
const accessorTarget = { value: 3 };
const accessorChecksum = readAcrossInstallation(accessorTarget, 64);

function fixedRabLoop(view, rounds) {
  let checksum = 0;
  for (let index = 0; index < rounds; index++) checksum += view[index & 3];
  return checksum;
}
const rab = new ArrayBuffer(16, { maxByteLength: 32 });
const fixedView = new Int32Array(rab, 0, 4);
fixedView.set([1, 2, 3, 4]);
for (let warm = 0; warm < 5000; warm++) fixedRabLoop(fixedView, warm < 8 ? 4 : 0);
const rabFull = fixedRabLoop(fixedView, 64);
rab.resize(4);
const rabShrunk = fixedRabLoop(fixedView, 64);
rab.resize(16);
fixedView.set([5, 6, 7, 8]);
const rabGrown = fixedRabLoop(fixedView, 64);

let reentries = 0;
function readWithReentry(receiver, callback, rounds) {
  let checksum = 0;
  for (let index = 0; index < rounds; index++) {
    checksum += receiver.value;
    if (index === 31) callback(receiver);
  }
  return checksum;
}
function mutateAndAllocate(receiver) {
  reentries++;
  receiver.value = 11;
  const roots = [];
  for (let index = 0; index < 256; index++) {
    roots.push({ index, payload: "licm-" + index });
  }
  return roots[255].payload;
}
const reentryTarget = { value: 1 };
const reentryChecksum = readWithReentry(reentryTarget, mutateAndAllocate, 64);

const step8Result = JSON.stringify({
  packedChecksum,
  packedOutput,
  changedShape,
  accessorChecksum,
  accessorReads,
  rab: [rabFull, rabShrunk, rabGrown],
  reentryChecksum,
  reentries,
  reentryValue: reentryTarget.value,
});
console.log(step8Result);
step8Result;
