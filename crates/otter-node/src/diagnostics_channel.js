'use strict';
// `node:diagnostics_channel` — named pub/sub channels.

const channels = new Map();

class Channel {
  constructor(name) {
    this.name = name;
    this._subscribers = [];
  }
  get hasSubscribers() { return this._subscribers.length > 0; }
  subscribe(onMessage) { this._subscribers.push(onMessage); }
  unsubscribe(onMessage) {
    const i = this._subscribers.indexOf(onMessage);
    if (i === -1) return false;
    this._subscribers.splice(i, 1);
    return true;
  }
  publish(message) {
    for (const sub of this._subscribers.slice()) {
      try { sub(message, this.name); } catch { /* swallow subscriber errors */ }
    }
  }
  bindStore() {}
  unbindStore() { return false; }
  runStores(_data, fn, thisArg, ...args) { return fn.apply(thisArg, args); }
}

function channel(name) {
  let ch = channels.get(name);
  if (!ch) { ch = new Channel(name); channels.set(name, ch); }
  return ch;
}

function hasSubscribers(name) {
  const ch = channels.get(name);
  return !!ch && ch.hasSubscribers;
}

function subscribe(name, onMessage) { channel(name).subscribe(onMessage); }
function unsubscribe(name, onMessage) {
  const ch = channels.get(name);
  return ch ? ch.unsubscribe(onMessage) : false;
}

class TracingChannel {
  constructor(nameOrChannels) {
    const base = typeof nameOrChannels === 'string' ? nameOrChannels : null;
    const mk = (suffix, given) => given || (base ? channel(`tracing:${base}:${suffix}`) : new Channel(suffix));
    const c = typeof nameOrChannels === 'object' ? nameOrChannels : {};
    this.start = mk('start', c.start);
    this.end = mk('end', c.end);
    this.asyncStart = mk('asyncStart', c.asyncStart);
    this.asyncEnd = mk('asyncEnd', c.asyncEnd);
    this.error = mk('error', c.error);
  }
  get hasSubscribers() {
    return this.start.hasSubscribers || this.end.hasSubscribers ||
      this.asyncStart.hasSubscribers || this.asyncEnd.hasSubscribers || this.error.hasSubscribers;
  }
  traceSync(fn, context, thisArg, ...args) {
    this.start.publish(context);
    try { const r = fn.apply(thisArg, args); this.end.publish(context); return r; }
    catch (err) { context && (context.error = err); this.error.publish(context); this.end.publish(context); throw err; }
  }
  tracePromise(fn, context, thisArg, ...args) {
    this.start.publish(context);
    try {
      return Promise.resolve(fn.apply(thisArg, args)).then(
        (r) => { this.asyncStart.publish(context); this.asyncEnd.publish(context); this.end.publish(context); return r; },
        (err) => { context && (context.error = err); this.error.publish(context); this.end.publish(context); throw err; });
    } catch (err) { context && (context.error = err); this.error.publish(context); this.end.publish(context); throw err; }
  }
  traceCallback(fn, position, context, thisArg, ...args) {
    this.start.publish(context);
    return fn.apply(thisArg, args);
  }
}

function tracingChannel(nameOrChannels) { return new TracingChannel(nameOrChannels); }

module.exports = { channel, hasSubscribers, subscribe, unsubscribe, tracingChannel, Channel };
