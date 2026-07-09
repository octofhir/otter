// URL.prototype.searchParams — the JS half of the native URL class.
//
// Spec ([SameObject]): every URL instance exposes one stable
// URLSearchParams whose mutations reflect into the URL record. The
// native class owns the record; this glue caches one wrapper per
// instance and writes mutations back through the live `search`
// accessor. Known gap: assigning `url.search` directly does not
// refresh an already-created searchParams object.
(function () {
  'use strict';
  const cache = new WeakMap();
  // Factory keeps each wrapped method's `original` in its own scope.
  // (Also sidesteps the engine's per-iteration capture bug for consts
  // declared inside loop bodies — see memory: for-of derived-const
  // captures collapse to the last iteration.)
  function writeBack(url, params, original) {
    return function (...args) {
      const result = original(...args);
      url.search = params.toString();
      return result;
    };
  }
  Object.defineProperty(URL.prototype, 'searchParams', {
    configurable: true,
    enumerable: false,
    get() {
      let params = cache.get(this);
      if (params) return params;
      const url = this;
      // Constructed lazily so the URLSearchParams lazy global
      // materializes only when searchParams is actually touched.
      params = new URLSearchParams(url.search);
      for (const method of ['append', 'delete', 'set', 'sort']) {
        Object.defineProperty(params, method, {
          value: writeBack(url, params, params[method].bind(params)),
          writable: true,
          configurable: true,
        });
      }
      cache.set(this, params);
      return params;
    },
  });
})();
