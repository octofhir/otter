'use strict';
// Web-stream bridging is reached only through `toWeb` / `fromWeb`; until a
// full adapter exists those surface a clear capability error.
function unimplemented() {
  const err = new Error('web-stream adapters are not implemented');
  err.code = 'ERR_METHOD_NOT_IMPLEMENTED';
  throw err;
}
module.exports = {
  newStreamReadableFromReadableStream: unimplemented,
  newReadableStreamFromStreamReadable: unimplemented,
  newStreamWritableFromWritableStream: unimplemented,
  newWritableStreamFromStreamWritable: unimplemented,
  newStreamDuplexFromReadableWritablePair: unimplemented,
  newReadableWritablePairFromDuplex: unimplemented,
};
