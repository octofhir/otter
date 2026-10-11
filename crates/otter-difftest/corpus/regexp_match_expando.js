// Match result named properties retain spec creation order and shaped reads.
let checks = 0;
function check(value, label) { if (!value) throw new Error(label); checks++; }
const regexp = /(?<word>a)(?<tail>b)?/d;
function match(input) { return regexp.exec(input); }
function named(result) { return result.index + result.input.length; }
for (let i = 0; i < 6000; i++) named(match("za"));
const result = match("za");
check(Array.isArray(result) && result.length === 3 && result[0] === "a"
  && result[1] === "a" && result[2] === undefined, "capture array");
check(Object.keys(result).join() === "0,1,2,index,input,groups,indices", "match named birth order");
check(Object.getOwnPropertyNames(result).join() === "0,1,2,length,index,input,groups,indices",
  "match own names");
check(result.index === 1 && result.input === "za" && named(result) === 3, "hot match named reads");
check(Object.getPrototypeOf(result) === Array.prototype, "match prototype");
check(Object.getPrototypeOf(result.groups) === null && result.groups.word === "a"
  && result.groups.tail === undefined, "named groups");
check(Array.isArray(result.indices) && result.indices[0].join() === "1,2"
  && result.indices[1].join() === "1,2" && result.indices[2] === undefined, "indices array");
check(Object.keys(result.indices).join() === "0,1,2,groups"
  && Object.getOwnPropertyNames(result.indices).join() === "0,1,2,length,groups", "indices bag order");
check(Object.getPrototypeOf(result.indices.groups) === null
  && result.indices.groups.word === result.indices[1]
  && result.indices.groups.tail === undefined, "indices groups share pairs");
const equalRanges = /(?<outer>(?<inner>a))/d.exec("a");
check(equalRanges.indices.groups.outer === equalRanges.indices[1]
  && equalRanges.indices.groups.inner === equalRanges.indices[2]
  && equalRanges.indices[1] !== equalRanges.indices[2],
  "equal ranges retain distinct capture identities");
for (const key of ["index", "input", "groups", "indices"]) {
  const descriptor = Object.getOwnPropertyDescriptor(result, key);
  check(descriptor.writable && descriptor.enumerable && descriptor.configurable, "match descriptor");
}
delete result.input;
result.input = "replacement";
check(Object.keys(result).join() === "0,1,2,index,groups,indices,input", "match recreation order");
let receiver, calls = 0;
Object.defineProperty(result, "index", {configurable: true, enumerable: true,
  get() { receiver = this; calls++; return 5; }});
const proxy = new Proxy(result, {});
check(proxy.index === 5 && receiver === proxy && calls === 1, "match accessor receiver");
const plain = /a/.exec("za");
check(Object.keys(plain).join() === "0,index,input,groups" && plain.groups === undefined
  && !("indices" in plain), "match without indices or named groups");
Object.defineProperty(plain, "index", {value: 91, writable: false});
Object.setPrototypeOf(plain, null);
delete plain.input;
let recordChecksum = 0;
for (let i = 0; i < 6000; i++) {
  const plainRecord = /a/.exec("za");
  const indexedRecord = match("zab");
  recordChecksum += plainRecord.index + indexedRecord.index;
  if (i === 5999) {
    check(Object.getPrototypeOf(plainRecord) === Array.prototype
      && plainRecord.input === "za" && plainRecord.groups === undefined
      && !("indices" in plainRecord)
      && Object.keys(plainRecord).join() === "0,index,input,groups",
      "retained plain layout creates independent records");
    check(indexedRecord.groups.tail === "b"
      && indexedRecord.indices.groups.tail === indexedRecord.indices[2]
      && Object.keys(indexedRecord).join() === "0,1,2,index,input,groups,indices"
      && Object.keys(indexedRecord.indices).join() === "0,1,2,groups",
      "retained indexed layouts keep values and key order");
  }
}
check(recordChecksum === 12000 && plain.index === 91 && !("input" in plain),
  "mutated match never changes retained birth layouts");
const unnamedIndexed = /a/d.exec("za");
check(unnamedIndexed.groups === undefined && unnamedIndexed.indices.groups === undefined
  && Object.hasOwn(unnamedIndexed, "groups") && Object.hasOwn(unnamedIndexed.indices, "groups")
  && Object.keys(unnamedIndexed.indices).join() === "0,groups",
  "indexed retained layouts publish own undefined groups");
const stable = match("zab");
stable.self = stable;
stable.indices.self = stable;
const retained = [];
for (let i = 0; i < 1024; i++) retained.push(match("zab"));
check(stable.self === stable && stable.indices.self === stable
  && stable.indices.groups.tail === stable.indices[2] && stable.groups.tail === "b"
  && retained[1023].index === 1, "match bags and pair identities survive pressure");
console.log("regexp-match-expando", checks, calls);
