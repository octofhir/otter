'use strict';
// URLPattern (https://urlpattern.spec.whatwg.org/), implemented over the
// engine's own RegExp — no external dependency. Each URL component pattern
// (protocol, username, password, hostname, port, pathname, search, hash) is
// compiled to a RegExp with capture groups; `test` / `exec` match a URL's
// components against them.
//
// Pattern syntax handled: literal text (with `\` escapes), `:name` named
// groups, `:name(regex)` / `(regex)` custom groups, `*` full wildcards, and a
// trailing `?` optional modifier. A named group's default matcher is a segment
// wildcard bounded by the component's separator (`/` for pathname, `.` for
// hostname), else a full wildcard.

(function installUrlPattern(global) {
  'use strict';

  function def(name, value) {
    Object.defineProperty(global, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  }

  const COMPONENTS = [
    'protocol', 'username', 'password', 'hostname',
    'port', 'pathname', 'search', 'hash',
  ];
  // Segment separator per component (empty = full wildcard for `:name`).
  const SEPARATORS = {
    protocol: '', username: '', password: '', hostname: '.',
    port: '', pathname: '/', search: '', hash: '',
  };
  const REGEXP_META = /[.*+?^${}()|[\]\\]/g;

  function escapeLiteral(ch) {
    return ch.replace(REGEXP_META, '\\$&');
  }

  const NAME_CHAR = /[A-Za-z0-9_$]/;

  // Read a balanced `(...)` group starting at `src[i] === '('`, returning the
  // inner regex source and the index just past the closing paren.
  function readParenGroup(src, i) {
    let depth = 0;
    let out = '';
    for (; i < src.length; i++) {
      const c = src[i];
      if (c === '\\') { out += c + (src[i + 1] || ''); i++; continue; }
      if (c === '(') { depth++; if (depth === 1) continue; }
      if (c === ')') { depth--; if (depth === 0) return [out, i + 1]; }
      out += c;
    }
    throw new TypeError('URLPattern: unbalanced "(" in pattern');
  }

  // Compile one component pattern string to `{ regexp, names }`.
  function compileComponent(source, separator) {
    const segment = separator ? '[^' + escapeLiteral(separator) + ']+?' : '[^]+?';
    const names = [];
    let regex = '';
    let i = 0;
    let anonymous = 0;
    while (i < source.length) {
      const c = source[i];
      if (c === '\\') {
        regex += escapeLiteral(source[i + 1] || '');
        i += 2;
        continue;
      }
      if (c === ':') {
        i++;
        let name = '';
        while (i < source.length && NAME_CHAR.test(source[i])) { name += source[i]; i++; }
        if (name === '') throw new TypeError('URLPattern: named group is missing a name');
        let sub = segment;
        if (source[i] === '(') { const g = readParenGroup(source, i); sub = g[0]; i = g[1]; }
        names.push(name);
        regex += '(' + sub + ')';
      } else if (c === '(') {
        const g = readParenGroup(source, i);
        names.push(String(anonymous++));
        regex += '(' + g[0] + ')';
        i = g[1];
      } else if (c === '*') {
        names.push(String(anonymous++));
        regex += '(.*)';
        i++;
      } else {
        regex += escapeLiteral(c);
        i++;
      }
      // Optional modifier on the group just emitted.
      if (source[i] === '?') { regex += '?'; i++; }
    }
    return { regexp: new RegExp('^' + regex + '$'), names, pattern: source };
  }

  // Split a URLPattern constructor string into per-component patterns. A best
  // effort structural parse: protocol `://`, authority (user:pass@host:port),
  // path, `?search`, `#hash`. Absent components default to `*`.
  function parsePatternString(input) {
    const init = {};
    let rest = input;
    const hash = rest.indexOf('#');
    if (hash >= 0) { init.hash = rest.slice(hash + 1); rest = rest.slice(0, hash); }
    const search = rest.indexOf('?');
    if (search >= 0) { init.search = rest.slice(search + 1); rest = rest.slice(0, search); }
    const scheme = rest.indexOf('://');
    if (scheme >= 0) {
      init.protocol = rest.slice(0, scheme);
      rest = rest.slice(scheme + 3);
      let authority = rest;
      const slash = rest.indexOf('/');
      if (slash >= 0) { authority = rest.slice(0, slash); init.pathname = rest.slice(slash); }
      else init.pathname = '/';
      const at = authority.indexOf('@');
      if (at >= 0) {
        const creds = authority.slice(0, at);
        authority = authority.slice(at + 1);
        const colon = creds.indexOf(':');
        if (colon >= 0) { init.username = creds.slice(0, colon); init.password = creds.slice(colon + 1); }
        else init.username = creds;
      }
      const portColon = authority.lastIndexOf(':');
      if (portColon >= 0 && !authority.slice(portColon + 1).includes(']')) {
        init.hostname = authority.slice(0, portColon);
        init.port = authority.slice(portColon + 1);
      } else {
        init.hostname = authority;
      }
    } else {
      // Relative pattern — everything left is the pathname.
      init.pathname = rest;
    }
    return init;
  }

  function componentInputs(url) {
    // `url` is a URL instance; expose each component the way exec matches it.
    const protocol = url.protocol.endsWith(':') ? url.protocol.slice(0, -1) : url.protocol;
    return {
      protocol,
      username: url.username,
      password: url.password,
      hostname: url.hostname,
      port: url.port,
      pathname: url.pathname,
      search: url.search.startsWith('?') ? url.search.slice(1) : url.search,
      hash: url.hash.startsWith('#') ? url.hash.slice(1) : url.hash,
    };
  }

  const kCompiled = Symbol('compiled');

  class URLPattern {
    constructor(input, baseURL) {
      let init;
      if (input === undefined) {
        init = {};
      } else if (typeof input === 'string') {
        let source = input;
        if (baseURL !== undefined) {
          // Resolve the pattern against a base to fill absent components.
          const base = new URL(baseURL);
          const baseInit = parsePatternString(source);
          init = Object.assign(parsePatternString(base.href), baseInit);
          source = null;
        } else {
          init = parsePatternString(source);
        }
      } else if (input !== null && typeof input === 'object') {
        if (baseURL !== undefined) {
          throw new TypeError('URLPattern: a baseURL cannot be given with an init object');
        }
        init = {};
        if (input.baseURL !== undefined) {
          const base = new URL(input.baseURL);
          Object.assign(init, parsePatternString(base.href));
        }
        for (const name of COMPONENTS) {
          if (input[name] !== undefined) init[name] = String(input[name]);
        }
      } else {
        throw new TypeError('URLPattern: input must be a string or an object');
      }

      const compiled = {};
      for (const name of COMPONENTS) {
        const pattern = init[name] !== undefined ? init[name] : '*';
        compiled[name] = compileComponent(pattern, SEPARATORS[name]);
      }
      Object.defineProperty(this, kCompiled, { value: compiled, enumerable: false });
    }

    get protocol() { return this[kCompiled].protocol.pattern; }
    get username() { return this[kCompiled].username.pattern; }
    get password() { return this[kCompiled].password.pattern; }
    get hostname() { return this[kCompiled].hostname.pattern; }
    get port() { return this[kCompiled].port.pattern; }
    get pathname() { return this[kCompiled].pathname.pattern; }
    get search() { return this[kCompiled].search.pattern; }
    get hash() { return this[kCompiled].hash.pattern; }
    get hasRegExpGroups() {
      return COMPONENTS.some((name) => /[:(*]/.test(this[kCompiled][name].pattern));
    }

    #resolve(input, baseURL) {
      if (typeof input === 'string' || input === undefined) {
        const url = baseURL !== undefined
          ? new URL(input === undefined ? '' : input, baseURL)
          : new URL(input === undefined ? '' : input);
        return componentInputs(url);
      }
      // Init-object input: use the provided components, absent ones as ''.
      const out = {};
      for (const name of COMPONENTS) {
        out[name] = input[name] !== undefined ? String(input[name]) : '';
      }
      return out;
    }

    test(input, baseURL) {
      let inputs;
      try {
        inputs = this.#resolve(input, baseURL);
      } catch (_) {
        return false;
      }
      const compiled = this[kCompiled];
      for (const name of COMPONENTS) {
        if (!compiled[name].regexp.test(inputs[name])) return false;
      }
      return true;
    }

    exec(input, baseURL) {
      let inputs;
      try {
        inputs = this.#resolve(input, baseURL);
      } catch (_) {
        return null;
      }
      const compiled = this[kCompiled];
      const result = { inputs: [input === undefined ? {} : input] };
      if (baseURL !== undefined) result.inputs.push(baseURL);
      for (const name of COMPONENTS) {
        const c = compiled[name];
        const match = c.regexp.exec(inputs[name]);
        if (match === null) return null;
        const groups = {};
        for (let g = 0; g < c.names.length; g++) {
          groups[c.names[g]] = match[g + 1];
        }
        result[name] = { input: inputs[name], groups };
      }
      return result;
    }
  }
  Object.defineProperty(URLPattern.prototype, Symbol.toStringTag, {
    value: 'URLPattern', writable: false, enumerable: false, configurable: true,
  });
  def('URLPattern', URLPattern);
})(globalThis);
