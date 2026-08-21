'use strict';

// The program's timers are Node's timers.
//
// Node keeps its timer lists in JavaScript and asks the loop for one timer at
// a time; the engine's own timers are what the loop is here. They are handed
// to the module that drives them now, while these names still mean them —
// afterwards the names answer from an accessor that loads that very module.
//
// The module itself is loaded the first time a program asks for a timer, so
// nothing loads it during startup.
{
  // Kept in this closure rather than anywhere a program could see: the engine's
  // timers are not part of the surface a Node program is given.
  const engine = {
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout,
    setImmediate: globalThis.setImmediate,
    clearImmediate: globalThis.clearImmediate,
    setRef: globalThis.__otterTimerSetRef,
  };

  const names = [
    'setTimeout', 'clearTimeout',
    'setInterval', 'clearInterval',
    'setImmediate', 'clearImmediate',
  ];

  // The six arrive together: a program that clears a timer must be clearing
  // one the same module handed it.
  const install = () => {
    const timers = process.getBuiltinModule('timers');
    for (const name of names) {
      Object.defineProperty(globalThis, name, {
        value: timers[name],
        writable: true,
        enumerable: false,
        configurable: true,
      });
    }
    return timers;
  };

  for (const name of names) {
    const get = () => install()[name];
    // Left where the module that drives the timers can find them: it may be
    // asked for before any of these names is, and reading one then would ask
    // for it again.
    get.engineTimers = engine;
    Object.defineProperty(globalThis, name, {
      get,
      set(value) {
        install();
        Object.defineProperty(globalThis, name, {
          value,
          writable: true,
          enumerable: false,
          configurable: true,
        });
      },
      enumerable: false,
      configurable: true,
    });
  }
}
