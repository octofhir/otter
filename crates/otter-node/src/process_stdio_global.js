'use strict';

// Asking for input is what starts it flowing: `openStdin` is the older spelling
// of `process.stdin.resume()`, and answers the same stream.
process.openStdin = function openStdin() {
  const stdin = process.stdin;
  stdin.resume();
  return stdin;
};
