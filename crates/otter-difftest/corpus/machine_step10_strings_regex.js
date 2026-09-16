// Step 10 widening: every ordinary string representation may move in the
// nursery, generated string paths must reload after allocating safepoints, and
// successful RegExp work must remain identical across tiers and GC stress.

function churn(seed) {
  let last = null;
  for (let i = 0; i < 8; i++) {
    last = { label: "garbage-" + seed + "-" + i, values: [seed, i] };
  }
  return last.values[0] + last.values[1];
}

function inspectAfterAllocation(value, needle) {
  const held = value;
  const noise = churn(value.length);
  return [
    held.length,
    held.charCodeAt(0),
    held.charCodeAt(held.length - 1),
    held.indexOf(needle),
    held.startsWith(value.slice(0, 1)),
    held.endsWith(value.slice(-1)),
    noise,
  ];
}

function concatAcrossSafepoint(value, leftPrimitive) {
  churn(value.length);
  return leftPrimitive ? false + value : value + true;
}

function regexAfterAllocation(subject) {
  const capture = /([a-z]+)-(\d+)/g;
  const held = subject;
  churn(subject.length);
  const matches = [];
  let match;
  while ((match = capture.exec(held)) !== null) {
    matches.push([match[0], match[1], match[2], match.index]);
  }
  return {
    matches,
    replaced: held.replace(/[0-9]+/g, "#"),
    split: held.split(/-/),
    search: held.search(/beta/),
    test: /alpha-\d+/.test(held),
  };
}

const inlineLatin1 = "tiny";
const sequentialLatin1 = "x".repeat(96);
const inlineWide = "λx";
const sequentialWide = "λ".repeat(64);
const cons = "abcdefghijklmnop" + "qrstuvwxyz012345";
const slicedLatin1 = sequentialLatin1.slice(7, 73);
const slicedWide = sequentialWide.slice(5, 45);
// SetFunctionName first consults the callable's virtual `name` descriptor.
// That lookup allocates, so the inferred young name must already be rooted.
const inferredName = { ["computed" + "Name"]: function () {} }.computedName;

// The string helper reaches Template at 79 entries and Machine at 148 entries.
// Seven calls per outer iteration make 64 iterations a bounded margin above
// both thresholds without turning full GC verification into a minute-scale run.
for (let warm = 0; warm < 64; warm++) {
  inspectAfterAllocation(inlineLatin1, "in");
  inspectAfterAllocation(sequentialLatin1, "xxx");
  inspectAfterAllocation(inlineWide, "x");
  inspectAfterAllocation(sequentialWide, "λλ");
  inspectAfterAllocation(cons, "mnopq");
  inspectAfterAllocation(slicedLatin1, "xxxx");
  inspectAfterAllocation(slicedWide, "λλλ");
  concatAcrossSafepoint(inlineLatin1, (warm & 1) === 0);
}

const step10Result = JSON.stringify({
  inlineLatin1: inspectAfterAllocation(inlineLatin1, "in"),
  sequentialLatin1: inspectAfterAllocation(sequentialLatin1, "xxx"),
  inlineWide: inspectAfterAllocation(inlineWide, "x"),
  sequentialWide: inspectAfterAllocation(sequentialWide, "λλ"),
  cons: inspectAfterAllocation(cons, "mnopq"),
  slicedLatin1: inspectAfterAllocation(slicedLatin1, "xxxx"),
  slicedWide: inspectAfterAllocation(slicedWide, "λλλ"),
  concat: [
    concatAcrossSafepoint("left", false),
    concatAcrossSafepoint("right", true),
  ],
  inferredName: inferredName.name,
  regexp: regexAfterAllocation("alpha-12 beta-345 gamma-6"),
});

console.log(step10Result);
step10Result;
