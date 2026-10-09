const leafOriginal = String.prototype.charCodeAt;
let armed = false;
function leafEvaluate(index, receiver, child) {
  try { if (armed) { armed = false; gc(); String.prototype.charCodeAt = function leafReplacement() { return 909; }; } } catch (e) { throw e; }
  return index;
}
function leafObserve(result, receiver, index, child) { try { return result; } catch (e) { throw e; } }
function leafWorker(receiver, key, index, child) {
  const result = receiver[key](leafEvaluate(index, receiver, child));
  leafObserve(result, receiver, index, child);
  return result;
}
const leafWarmChild = {marker: 731};
for (let warm = 0; warm < 20000; warm++) leafWorker('qz', 'charCodeAt', 1, leafWarmChild);
const leafYoungText = ['q', 'z'].join('');
armed = true;
const moved = leafWorker(leafYoungText, 'charCodeAt', 1, leafWarmChild);
const changed = 'qz'.charCodeAt(1);
String.prototype.charCodeAt = leafOriginal;
console.log(JSON.stringify([moved, changed]));
