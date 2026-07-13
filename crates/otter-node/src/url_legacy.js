// Dedicated implementation of Node's original `url` API. This is deliberately
// separate from WHATWG URL: legacy parsing and RFC 2396 resolution have
// observable object-shape, escaping, and relative-path semantics of their own.
//
// Adapted from Node.js v24 `lib/url.js` (MIT). Invariants: parsed query objects
// have a null prototype; resolution never consults the filesystem; URL fields
// are normalized before `href` is reconstructed through `Url#format`.

const legacyProtocolPattern = /^[a-z0-9.+-]+:/i;
const legacyPortPattern = /:[0-9]*$/;
const legacyHostPattern = /^\/\/[^@/]+@[^@/]+/;
const legacySimplePathPattern = /^(\/\/?(?!\/)[^?\s]*)(\?[^\s]*)?$/;
const legacySlashedProtocols = new Set([
  'http', 'http:', 'https', 'https:', 'ftp', 'ftp:', 'gopher', 'gopher:',
  'file', 'file:', 'ws', 'ws:', 'wss', 'wss:',
]);
const legacyHostlessProtocols = new Set(['javascript', 'javascript:']);
const legacyUnsafeProtocols = new Set(['javascript', 'javascript:']);

function Url() {
  this.protocol = null;
  this.slashes = null;
  this.auth = null;
  this.host = null;
  this.port = null;
  this.hostname = null;
  this.hash = null;
  this.search = null;
  this.query = null;
  this.pathname = null;
  this.path = null;
  this.href = null;
}

function legacyQueryDecode(value) {
  value = value.split('+').join(' ');
  try { return decodeURIComponent(value); } catch { return value; }
}

function legacyQueryParse(query) {
  const result = Object.create(null);
  if (!query) return result;
  for (const part of query.split('&')) {
    if (!part) continue;
    const equals = part.indexOf('=');
    const key = legacyQueryDecode(equals < 0 ? part : part.slice(0, equals));
    const value = legacyQueryDecode(equals < 0 ? '' : part.slice(equals + 1));
    if (result[key] === undefined) result[key] = value;
    else if (Array.isArray(result[key])) result[key].push(value);
    else result[key] = [result[key], value];
  }
  return result;
}

function legacyQueryPrimitive(value) {
  if (typeof value === 'string') return value;
  if (typeof value === 'number' && Number.isFinite(value)) return String(value);
  if (typeof value === 'bigint') return String(value);
  if (typeof value === 'boolean') return value ? 'true' : 'false';
  return '';
}

function legacyQueryStringify(query) {
  const fields = [];
  for (const key of Object.keys(query)) {
    const values = Array.isArray(query[key]) ? query[key] : [query[key]];
    for (const value of values) {
      fields.push(`${encodeURIComponent(key)}=${encodeURIComponent(legacyQueryPrimitive(value))}`);
    }
  }
  return fields.join('&');
}

function legacyIsIpv6Hostname(hostname) {
  return hostname[0] === '[' && hostname[hostname.length - 1] === ']';
}

function legacyAutoEscape(rest) {
  let output = '';
  for (let index = 0; index < rest.length; index++) {
    const char = rest[index];
    switch (char) {
      case '\t': output += '%09'; break;
      case '\n': output += '%0A'; break;
      case '\r': output += '%0D'; break;
      case ' ': output += '%20'; break;
      case '"': output += '%22'; break;
      case "'": output += '%27'; break;
      case '<': output += '%3C'; break;
      case '>': output += '%3E'; break;
      case '\\': output += '%5C'; break;
      case '^': output += '%5E'; break;
      case '`': output += '%60'; break;
      case '{': output += '%7B'; break;
      case '|': output += '%7C'; break;
      case '}': output += '%7D'; break;
      default: output += char;
    }
  }
  return output;
}

function legacyGetHostname(target, rest, hostname) {
  for (let index = 0; index < hostname.length; index++) {
    if ('/\\#?:'.includes(hostname[index])) {
      target.hostname = hostname.slice(0, index);
      return `/${hostname.slice(index)}${rest}`;
    }
  }
  return rest;
}

function legacyParse(input, parseQueryString, slashesDenoteHost) {
  if (input instanceof Url) return input;
  const value = new Url();
  return value.parse(input, parseQueryString, slashesDenoteHost);
}

Url.prototype.parse = function parse(input, parseQueryString, slashesDenoteHost) {
  if (typeof input !== 'string') throw invalidArgType('url', input);

  let hasHash = false;
  let hasAt = false;
  let split = false;
  let rest = '';
  let last = 0;
  let start = -1;
  let end = -1;
  let inWhitespace = false;
  for (let index = 0; index < input.length; index++) {
    const code = input.charCodeAt(index);
    const whitespace = code < 33 || code === 160 || code === 65279;
    if (start === -1) {
      if (whitespace) continue;
      start = index;
      last = index;
    } else if (inWhitespace) {
      if (!whitespace) { end = -1; inWhitespace = false; }
    } else if (whitespace) {
      end = index;
      inWhitespace = true;
    }
    if (!split) {
      if (input[index] === '@') hasAt = true;
      else if (input[index] === '#' || input[index] === '?') {
        if (input[index] === '#') hasHash = true;
        split = true;
      } else if (input[index] === '\\') {
        if (index > last) rest += input.slice(last, index);
        rest += '/';
        last = index + 1;
      }
    } else if (!hasHash && input[index] === '#') {
      hasHash = true;
    }
  }
  if (start !== -1) {
    if (last === start) rest = input.slice(start, end === -1 ? undefined : end);
    else if (end === -1 && last < input.length) rest += input.slice(last);
    else if (end !== -1 && last < end) rest += input.slice(last, end);
  }

  if (!slashesDenoteHost && !hasHash && !hasAt) {
    const simple = legacySimplePathPattern.exec(rest);
    if (simple) {
      this.path = rest;
      this.href = rest;
      this.pathname = simple[1];
      if (simple[2]) {
        this.search = simple[2];
        this.query = parseQueryString ? legacyQueryParse(simple[2].slice(1)) : simple[2].slice(1);
      } else if (parseQueryString) {
        this.search = null;
        this.query = Object.create(null);
      }
      return this;
    }
  }

  const protocolMatch = legacyProtocolPattern.exec(rest);
  let lowerProtocol;
  if (protocolMatch) {
    lowerProtocol = protocolMatch[0].toLowerCase();
    this.protocol = lowerProtocol;
    rest = rest.slice(protocolMatch[0].length);
  }

  let slashes = false;
  if (slashesDenoteHost || protocolMatch || legacyHostPattern.test(rest)) {
    slashes = rest.startsWith('//');
    if (slashes && !(protocolMatch && legacyHostlessProtocols.has(lowerProtocol))) {
      rest = rest.slice(2);
      this.slashes = true;
    }
  }

  if (!legacyHostlessProtocols.has(lowerProtocol) &&
      (slashes || (protocolMatch && !legacySlashedProtocols.has(lowerProtocol)))) {
    let hostEnd = -1;
    let at = -1;
    let nonHost = -1;
    for (let index = 0; index < rest.length; index++) {
      const char = rest[index];
      if (char === '\t' || char === '\n' || char === '\r') {
        rest = rest.slice(0, index) + rest.slice(index + 1);
        index--;
      } else if (' \"%\';<>\\^`{|}'.includes(char)) {
        if (nonHost === -1) nonHost = index;
      } else if (char === '#' || char === '/' || char === '?') {
        if (nonHost === -1) nonHost = index;
        hostEnd = index;
      } else if (char === '@') {
        at = index;
        nonHost = -1;
      }
      if (hostEnd !== -1) break;
    }
    let hostStart = 0;
    if (at !== -1) {
      this.auth = decodeURIComponent(rest.slice(0, at));
      hostStart = at + 1;
    }
    if (nonHost === -1) {
      this.host = rest.slice(hostStart);
      rest = '';
    } else {
      this.host = rest.slice(hostStart, nonHost);
      rest = rest.slice(nonHost);
    }
    this.parseHost();
    if (typeof this.hostname !== 'string') this.hostname = '';
    const ipv6 = legacyIsIpv6Hostname(this.hostname);
    if (!ipv6) rest = legacyGetHostname(this, rest, this.hostname);
    if (this.hostname.length > 255) this.hostname = '';
    else this.hostname = this.hostname.toLowerCase();
    if (this.hostname && !ipv6) {
      const ascii = domainToASCII(this.hostname);
      if (ascii) this.hostname = ascii;
    }
    this.host = (this.hostname || '') + (this.port ? `:${this.port}` : '');
    if (ipv6) {
      this.hostname = this.hostname.slice(1, -1);
      if (!rest.startsWith('/')) rest = '/' + rest;
    }
  }

  if (!legacyUnsafeProtocols.has(lowerProtocol)) rest = legacyAutoEscape(rest);
  let question = -1;
  let hash = -1;
  for (let index = 0; index < rest.length; index++) {
    if (rest[index] === '#') { hash = index; this.hash = rest.slice(index); break; }
    if (rest[index] === '?' && question === -1) question = index;
  }
  if (question !== -1) {
    const queryEnd = hash === -1 ? rest.length : hash;
    this.search = rest.slice(question, queryEnd);
    this.query = rest.slice(question + 1, queryEnd);
    if (parseQueryString) this.query = legacyQueryParse(this.query);
  } else if (parseQueryString) {
    this.search = null;
    this.query = Object.create(null);
  }
  const separator = question !== -1 && (hash === -1 || question < hash) ? question : hash;
  if (separator === -1) {
    if (rest) this.pathname = rest;
  } else if (separator > 0) {
    this.pathname = rest.slice(0, separator);
  }
  if (legacySlashedProtocols.has(lowerProtocol) && this.hostname && !this.pathname) {
    this.pathname = '/';
  }
  if (this.pathname || this.search) this.path = (this.pathname || '') + (this.search || '');
  this.href = this.format();
  return this;
};

Url.prototype.parseHost = function parseHost() {
  let host = this.host;
  const match = legacyPortPattern.exec(host);
  if (match) {
    const port = match[0];
    if (port !== ':') this.port = port.slice(1);
    host = host.slice(0, host.length - port.length);
  }
  if (host) this.hostname = host;
};

Url.prototype.format = function formatLegacy() {
  let auth = this.auth || '';
  if (auth) auth = encodeURIComponent(auth).replace(/%3A/gi, ':') + '@';
  let protocol = this.protocol || '';
  if (protocol && !protocol.endsWith(':')) protocol += ':';
  let pathname = String(this.pathname || '').split('#').join('%23').split('?').join('%3F');
  let hash = this.hash || '';
  let host = '';
  if (this.host) host = auth + this.host;
  else if (this.hostname) {
    const hostname = this.hostname.includes(':') && !legacyIsIpv6Hostname(this.hostname) ?
      `[${this.hostname}]` : this.hostname;
    host = auth + hostname + (this.port ? `:${this.port}` : '');
  }
  const query = this.query !== null && typeof this.query === 'object' ?
    legacyQueryStringify(this.query) : '';
  let search = this.search || (query ? `?${query}` : '');
  if (this.slashes || legacySlashedProtocols.has(protocol)) {
    if (this.slashes || host) {
      if (pathname && !pathname.startsWith('/')) pathname = '/' + pathname;
      host = '//' + host;
    } else if (protocol.startsWith('file')) {
      host = '//';
    }
  }
  search = search.split('#').join('%23');
  if (hash && !hash.startsWith('#')) hash = '#' + hash;
  if (search && !search.startsWith('?')) search = '?' + search;
  return protocol + host + pathname + search + hash;
};

Url.prototype.resolve = function resolve(relative) {
  return this.resolveObject(legacyParse(relative, false, true)).format();
};

function legacyResolve(source, relative) {
  return legacyParse(source, false, true).resolve(relative);
}

function legacyResolveObject(source, relative) {
  if (!source) return relative;
  return legacyParse(source, false, true).resolveObject(relative);
}

Url.prototype.resolveObject = function resolveObject(relative) {
  if (typeof relative === 'string') relative = legacyParse(relative, false, true);
  const result = new Url();
  Object.assign(result, this);
  result.hash = relative.hash;
  if (relative.href === '') {
    result.href = result.format();
    return result;
  }
  if (relative.slashes && !relative.protocol) {
    for (const key of Object.keys(relative)) if (key !== 'protocol') result[key] = relative[key];
    if (legacySlashedProtocols.has(result.protocol) && result.hostname && !result.pathname) {
      result.path = result.pathname = '/';
    }
    result.href = result.format();
    return result;
  }
  if (relative.protocol && relative.protocol !== result.protocol) {
    if (!legacySlashedProtocols.has(relative.protocol)) {
      Object.assign(result, relative);
      result.href = result.format();
      return result;
    }
    result.protocol = relative.protocol;
    if (!relative.host && !/^file:?$/.test(relative.protocol) &&
        !legacyHostlessProtocols.has(relative.protocol)) {
      const path = (relative.pathname || '').split('/');
      while (path.length && !(relative.host = path.shift())) {}
      relative.host = relative.host || '';
      relative.hostname = relative.hostname || '';
      if (path[0] !== '') path.unshift('');
      if (path.length < 2) path.unshift('');
      result.pathname = path.join('/');
    } else result.pathname = relative.pathname;
    result.search = relative.search;
    result.query = relative.query;
    result.host = relative.host || '';
    result.auth = relative.auth;
    result.hostname = relative.hostname || relative.host;
    result.port = relative.port;
    if (result.pathname || result.search) result.path = (result.pathname || '') + (result.search || '');
    result.slashes = result.slashes || relative.slashes;
    result.href = result.format();
    return result;
  }

  const sourceAbsolute = result.pathname && result.pathname[0] === '/';
  const relativeAbsolute = relative.host || (relative.pathname && relative.pathname[0] === '/');
  let mustEndAbsolute = relativeAbsolute || sourceAbsolute || (result.host && relative.pathname);
  const removeAllDots = mustEndAbsolute;
  let sourcePath = result.pathname ? result.pathname.split('/') : [];
  const relativePath = relative.pathname ? relative.pathname.split('/') : [];
  const noLeadingSlashes = result.protocol && !legacySlashedProtocols.has(result.protocol);
  if (noLeadingSlashes) {
    result.hostname = '';
    result.port = null;
    if (result.host) {
      if (sourcePath[0] === '') sourcePath[0] = result.host;
      else sourcePath.unshift(result.host);
    }
    result.host = '';
    if (relative.protocol) {
      relative.hostname = null;
      relative.port = null;
      result.auth = null;
      if (relative.host) {
        if (relativePath[0] === '') relativePath[0] = relative.host;
        else relativePath.unshift(relative.host);
      }
      relative.host = null;
    }
    mustEndAbsolute = mustEndAbsolute && (relativePath[0] === '' || sourcePath[0] === '');
  }
  if (relativeAbsolute) {
    if (relative.host || relative.host === '') {
      if (result.host !== relative.host) result.auth = null;
      result.host = relative.host;
      result.port = relative.port;
    }
    if (relative.hostname || relative.hostname === '') {
      if (result.hostname !== relative.hostname) result.auth = null;
      result.hostname = relative.hostname;
    }
    result.search = relative.search;
    result.query = relative.query;
    sourcePath = relativePath;
  } else if (relativePath.length) {
    sourcePath.pop();
    sourcePath = sourcePath.concat(relativePath);
    result.search = relative.search;
    result.query = relative.query;
  } else if (relative.search !== null && relative.search !== undefined) {
    if (noLeadingSlashes) {
      result.hostname = result.host = sourcePath.shift();
      const auth = result.host && result.host.indexOf('@') > 0 ? result.host.split('@') : null;
      if (auth) { result.auth = auth.shift(); result.host = result.hostname = auth.shift(); }
    }
    result.search = relative.search;
    result.query = relative.query;
    if (result.pathname !== null || result.search !== null) {
      result.path = (result.pathname || '') + (result.search || '');
    }
    result.href = result.format();
    return result;
  }
  if (!sourcePath.length) {
    result.pathname = null;
    result.path = result.search ? '/' + result.search : null;
    result.href = result.format();
    return result;
  }
  let last = sourcePath[sourcePath.length - 1];
  const trailingSlash = ((result.host || relative.host || sourcePath.length > 1) &&
    (last === '.' || last === '..')) || last === '';
  let up = 0;
  for (let index = sourcePath.length - 1; index >= 0; index--) {
    last = sourcePath[index];
    if (last === '.') sourcePath.splice(index, 1);
    else if (last === '..') { sourcePath.splice(index, 1); up++; }
    else if (up) { sourcePath.splice(index, 1); up--; }
  }
  if (!mustEndAbsolute && !removeAllDots) while (up-- > 0) sourcePath.unshift('..');
  if (mustEndAbsolute && sourcePath[0] !== '' && (!sourcePath[0] || sourcePath[0][0] !== '/')) {
    sourcePath.unshift('');
  }
  if (trailingSlash && !sourcePath.join('/').endsWith('/')) sourcePath.push('');
  const absolute = sourcePath[0] === '' || (sourcePath[0] && sourcePath[0][0] === '/');
  if (noLeadingSlashes) {
    result.hostname = result.host = absolute ? '' : (sourcePath.length ? sourcePath.shift() : '');
    const auth = result.host && result.host.indexOf('@') > 0 ? result.host.split('@') : null;
    if (auth) { result.auth = auth.shift(); result.host = result.hostname = auth.shift(); }
  }
  mustEndAbsolute = mustEndAbsolute || (result.host && sourcePath.length);
  if (mustEndAbsolute && !absolute) sourcePath.unshift('');
  if (!sourcePath.length) result.pathname = result.path = null;
  else result.pathname = sourcePath.join('/');
  if (result.pathname !== null || result.search !== null) {
    result.path = (result.pathname || '') + (result.search || '');
  }
  result.auth = relative.auth || result.auth;
  result.slashes = result.slashes || relative.slashes;
  result.href = result.format();
  return result;
};
