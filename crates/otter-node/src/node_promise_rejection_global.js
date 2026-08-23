'use strict';

// What becomes of a promise nobody handled.
//
// The engine reports a rejection nobody took at the point the microtask queue
// empties, through one reporter on the global. This is Node's answer to one:
// `process` hears about it, and what the command line asked for decides what
// becomes of it. A runtime that also carries the web surface dispatches the
// `unhandledrejection` event first — a handler there may claim the rejection —
// and hands on what is left, which is this.
{
  // What Node calls error-like: a value carrying a stack. Node reads that as an
  // own property because V8 stamps one onto every error instance; this engine
  // answers `stack` from an accessor on `Error.prototype`, which is what
  // test262 asks of it, so the question is whether the value has a stack at
  // all rather than where it keeps it.
  const looksLikeError = (value) =>
    typeof value === 'object' && value !== null && typeof value.stack === 'string';

  // Node's `noSideEffectsToString`: what a reason is called in the message,
  // without running anything the value brought with it.
  const describe = (value) => {
    if (typeof value === 'string') return value;
    if (typeof value === 'bigint') return `${value}n`;
    if (typeof value === 'symbol') return value.toString();
    if (value === null || value === undefined || typeof value !== 'object') return String(value);
    if (looksLikeError(value)) return `${value.name}: ${value.message}`;
    return '#<Object>';
  };

  const unhandledError = (reason) => {
    const message = 'This error originated either by throwing inside of an ' +
      'async function without a catch block, or by rejecting a promise which ' +
      `was not handled with .catch(). The promise rejected with the reason "${describe(reason)}".`;
    const error = new Error(message);
    error.code = 'ERR_UNHANDLED_REJECTION';
    Object.defineProperty(error, 'name', {
      value: 'UnhandledPromiseRejection',
      writable: true,
      configurable: true,
    });
    return error;
  };

  // Two warnings per rejection, as Node emits them: the reason itself, and a
  // note saying where the warning came from and how to make it fatal. The note
  // borrows the reason's stack, because its own would only point back here.
  let rejectionId = 0;
  const warningName = 'UnhandledPromiseRejectionWarning';
  const warn = (reason) => {
    rejectionId += 1;
    const note = new Error(
      'Unhandled promise rejection. This error originated either by throwing ' +
      'inside of an async function without a catch block, or by rejecting a ' +
      'promise which was not handled with .catch(). To terminate the node ' +
      'process on unhandled promise rejection, use the CLI flag ' +
      '`--unhandled-rejections=strict` (see ' +
      'https://nodejs.org/api/cli.html#cli_unhandled_rejections_mode). ' +
      `(rejection id: ${rejectionId})`);
    note.name = warningName;
    if (looksLikeError(reason)) {
      note.stack = reason.stack;
      process.emitWarning(reason.stack, warningName);
    } else {
      process.emitWarning(describe(reason), warningName);
    }
    process.emitWarning(note);
  };

  // The switch is read the first time a rejection is reported: reading it at
  // startup would load the module that parses the command line before a
  // program has asked for anything.
  let mode;
  const rejectionMode = () => {
    if (mode === undefined) {
      mode = process.getBuiltinModule('internal/options')
        .getOptionValue('--unhandled-rejections') || 'throw';
    }
    return mode;
  };

  const fire = function (promise, reason, handled) {
    if (handled) {
      process.emit('rejectionHandled', promise);
      return;
    }

    const taken = process.emit('unhandledRejection', reason, promise);
    switch (rejectionMode()) {
      case 'none':
        return;
      case 'warn':
        warn(reason);
        return;
      case 'warn-with-error-code':
        if (!taken) {
          warn(reason);
          process.exitCode = 1;
        }
        return;
      case 'strict':
        throw looksLikeError(reason) ? reason : unhandledError(reason);
      default:
        // `throw`: a rejection nobody took ends the run, as an uncaught
        // exception the host names `'unhandledRejection'`.
        if (!taken) throw looksLikeError(reason) ? reason : unhandledError(reason);
    }
  };

  Object.defineProperty(globalThis, '__otterFirePromiseRejection', {
    value: fire,
    writable: true,
    enumerable: false,
    configurable: true,
  });
}
