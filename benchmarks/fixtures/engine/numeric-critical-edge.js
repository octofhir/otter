function engineNumericCriticalEdge(left, right) {
  let result = left;
  if (left < right) {
    result = left + right;
  }
  return result;
}
