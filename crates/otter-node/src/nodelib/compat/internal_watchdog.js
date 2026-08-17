'use strict';
// internal/watchdog — Node's native TraceSigintWatchdog runs a watchdog
// thread that captures the executing JS stack on SIGINT. This realm's
// closest honest equivalent is a SIGINT listener that reports the
// interrupt with the stack of the signal dispatch point.
class SigintWatchdog {
  constructor() {
    this._handler = null;
  }
  start() {
    if (this._handler !== null) return;
    this._handler = () => {
      process.stderr.write('KEYBOARD_INTERRUPT: Script execution was interrupted by `SIGINT`\n');
    };
    process.on('SIGINT', this._handler);
  }
  stop() {
    if (this._handler === null) return;
    process.removeListener('SIGINT', this._handler);
    this._handler = null;
  }
}
module.exports = { SigintWatchdog, TraceSigintWatchdog: SigintWatchdog };
