// Node-style constructor chain: Parent.call(this) + many field stores,
// symbol-keyed fields, then a subclass constructor that calls it.
const kA = Symbol('a'), kB = Symbol('b'), kC = Symbol('c'), kD = Symbol('d');
function EE() { this._events = { __proto__: null }; this._eventsCount = 0; this._maxListeners = undefined; }
function Stream() { EE.call(this); }
Object.setPrototypeOf(Stream.prototype, EE.prototype); Object.setPrototypeOf(Stream, EE);
function nop() {}
function OM(options) {
  Stream.call(this);
  this.outputData = []; this.outputSize = 0; this.writable = true; this.destroyed = false;
  this._last = false; this.chunkedEncoding = false; this.shouldKeepAlive = true;
  this.maxRequestsOnConnectionReached = false; this._defaultKeepAlive = true;
  this.useChunkedEncodingByDefault = true; this.sendDate = false; this._removedConnection = false;
  this._removedContLen = false; this._removedTE = false; this.strictContentLength = false;
  this[kA] = 0; this._contentLength = null; this._hasBody = true; this._trailer = ''; this[kB] = false;
  this.finished = false; this._headerSent = false; this[kC] = 0; this[kD] = []; this._closed = false;
  this._header = null; this._keepAliveTimeout = 0; this._onPendingData = nop;
  this.hwm = options?.highWaterMark ?? 16384;
}
Object.setPrototypeOf(OM.prototype, Stream.prototype); Object.setPrototypeOf(OM, Stream);
function SR(req, options) {
  OM.call(this, options);
  if (req.method === 'HEAD') this._hasBody = false;
  this.req = req; this.sendDate = true; this._sent100 = false; this._expect_continue = false;
  this.statusCode = 200; this.statusMessage = undefined;
}
Object.setPrototypeOf(SR.prototype, OM.prototype); Object.setPrototypeOf(SR, OM);
const req = { method: 'GET' };
let keep;
for (let i = 0; i < 2e5; i++) keep = new SR(req, {});
const N = 3e6;
const t0 = process.hrtime.bigint();
for (let i = 0; i < N; i++) keep = new SR(req, {});
console.log('new SR', (Number(process.hrtime.bigint() - t0) / N).toFixed(1), 'ns', keep.statusCode);
