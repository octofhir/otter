'use strict';
// `node:readline` — line-oriented Interface over input/output streams, plus the
// cursor/clear ANSI helpers. Terminal (raw keypress) mode is supported at a
// basic level; the common path is line mode driven by the input stream's
// 'data' events.

const EventEmitter = require('events');

class Interface extends EventEmitter {
  constructor(input, output, completer, terminal) {
    super();
    let options;
    if (input && typeof input === 'object' && (input.input || input.output || 'terminal' in input)) {
      options = input;
    } else {
      options = { input, output, completer, terminal };
    }
    this.input = options.input;
    this.output = options.output;
    this.completer = options.completer;
    this.terminal = !!options.terminal;
    this._prompt = options.prompt !== undefined ? options.prompt : '> ';
    this.historySize = options.historySize === undefined ? 30 : options.historySize;
    this.history = [];
    this.line = '';
    this.cursor = 0;
    this._buffer = '';
    this._questionCallback = null;
    this.closed = false;

    this._onData = (chunk) => this._normalWrite(chunk);
    if (this.input && typeof this.input.on === 'function') {
      this.input.on('data', this._onData);
      this.input.on('end', () => { if (!this.closed) this.close(); });
    }
  }

  _normalWrite(chunk) {
    if (chunk === null || chunk === undefined) return;
    let str = typeof chunk === 'string' ? chunk : chunk.toString('utf8');
    this._buffer += str;
    let index;
    while ((index = this._buffer.indexOf('\n')) !== -1) {
      let line = this._buffer.slice(0, index);
      if (line.endsWith('\r')) line = line.slice(0, -1);
      this._buffer = this._buffer.slice(index + 1);
      this._online(line);
    }
  }

  _online(line) {
    if (this._questionCallback) {
      const cb = this._questionCallback;
      this._questionCallback = null;
      cb(line);
    } else {
      if (line.length && this.history[0] !== line) {
        this.history.unshift(line);
        if (this.history.length > this.historySize) this.history.pop();
      }
      this.emit('line', line);
    }
  }

  setPrompt(prompt) { this._prompt = prompt; }
  getPrompt() { return this._prompt; }

  prompt(preserveCursor) {
    if (this.closed) return;
    if (this.output && typeof this.output.write === 'function') this.output.write(this._prompt);
  }

  question(query, options, cb) {
    if (typeof options === 'function') { cb = options; options = {}; }
    if (this.output && typeof this.output.write === 'function') this.output.write(query);
    if (cb) {
      this._questionCallback = cb;
      return undefined;
    }
    return new Promise((resolve) => { this._questionCallback = resolve; });
  }

  write(data, key) {
    if (this.closed) return;
    if (typeof data === 'string' && this.output && typeof this.output.write === 'function') {
      // In a non-terminal interface, writing feeds the input echo path.
      this.output.write(data);
    }
  }

  pause() {
    if (this.input && typeof this.input.pause === 'function') this.input.pause();
    this.emit('pause');
    return this;
  }

  resume() {
    if (this.input && typeof this.input.resume === 'function') this.input.resume();
    this.emit('resume');
    return this;
  }

  getCursorPos() { return { rows: 0, cols: this._prompt.length + this.cursor }; }

  close() {
    if (this.closed) return;
    this.closed = true;
    if (this.input && typeof this.input.removeListener === 'function') {
      this.input.removeListener('data', this._onData);
    }
    this.emit('close');
  }

  [Symbol.asyncIterator]() {
    const self = this;
    const lines = [];
    let waiting = null;
    let done = false;
    self.on('line', (l) => { if (waiting) { const w = waiting; waiting = null; w({ value: l, done: false }); } else lines.push(l); });
    self.on('close', () => { done = true; if (waiting) { const w = waiting; waiting = null; w({ value: undefined, done: true }); } });
    return {
      next() {
        if (lines.length) return Promise.resolve({ value: lines.shift(), done: false });
        if (done) return Promise.resolve({ value: undefined, done: true });
        return new Promise((resolve) => { waiting = resolve; });
      },
      [Symbol.asyncIterator]() { return this; },
    };
  }
}

function createInterface(input, output, completer, terminal) {
  return new Interface(input, output, completer, terminal);
}

// ---- cursor / clear helpers (write ANSI to a stream) ----
function writeSeq(stream, seq, cb) {
  let ok = true;
  if (stream && typeof stream.write === 'function') ok = stream.write(seq);
  if (typeof cb === 'function') process.nextTick ? process.nextTick(cb) : cb();
  return ok;
}

function cursorTo(stream, x, y, cb) {
  if (typeof y === 'function') { cb = y; y = undefined; }
  let seq;
  if (typeof y === 'number') seq = `\x1b[${y + 1};${x + 1}H`;
  else seq = `\x1b[${x + 1}G`;
  return writeSeq(stream, seq, cb);
}

function moveCursor(stream, dx, dy, cb) {
  let seq = '';
  if (dx < 0) seq += `\x1b[${-dx}D`; else if (dx > 0) seq += `\x1b[${dx}C`;
  if (dy < 0) seq += `\x1b[${-dy}A`; else if (dy > 0) seq += `\x1b[${dy}B`;
  return writeSeq(stream, seq, cb);
}

function clearLine(stream, dir, cb) {
  const seq = dir < 0 ? '\x1b[1K' : dir > 0 ? '\x1b[0K' : '\x1b[2K';
  return writeSeq(stream, seq, cb);
}

function clearScreenDown(stream, cb) {
  return writeSeq(stream, '\x1b[0J', cb);
}

function emitKeypressEvents(stream, iface) {
  // Minimal: forward 'data' bytes as keypress char events.
  if (stream._keypressDecoderAttached) return;
  stream._keypressDecoderAttached = true;
  stream.on('data', (chunk) => {
    const s = typeof chunk === 'string' ? chunk : chunk.toString('utf8');
    for (const ch of s) {
      stream.emit('keypress', ch, { sequence: ch, name: undefined, ctrl: false, meta: false, shift: false });
    }
  });
}

module.exports = {
  Interface,
  createInterface,
  cursorTo,
  moveCursor,
  clearLine,
  clearScreenDown,
  emitKeypressEvents,
};
module.exports.promises = { Interface, createInterface };
