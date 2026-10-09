function leafEvaluate(index, receiver, child) { return index; }
function leafObserve(result, receiver, index, child) { return result; }
function leafWorker(receiver, key, index, child) {
  const result = receiver[key](leafEvaluate(index, receiver, child));
  leafObserve(result, receiver, index, child);
  return result;
}
const leafWarmChild = {marker: 731};
for (let warm = 0; warm < 20000; warm++) leafWorker('qz', 'charCodeAt', 1, leafWarmChild);
console.log(leafWorker('qz', 'charCodeAt', 1, leafWarmChild));
