function engineKernel() {
  var state = 1 / 2;
  var hits = 0;
  var index = 1;
  var limit = 200000;
  while (index < limit) {
    var signed = -index;
    var remainder = signed % 17;
    var mixed = remainder + state;
    var power = (mixed * mixed) ** 1;
    var numeric = +(power - power);
    if (!numeric) {
      hits = hits + 1;
    }
    state = (power % 13) + 1 / 2;
    index = index + 1;
  }
  return hits;
}
