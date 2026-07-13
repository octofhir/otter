'use strict';

// Node's URL module is primarily a facade over the WHATWG globals. Otter's
// CLI installs those through otter-web; the small fallback keeps an embedder
// that opts into only otter-node functional for file URLs.
const URLCtor = globalThis.URL || class URL {
  constructor(input, base) {
    let href = String(input);
    if (base !== undefined && !/^[A-Za-z][A-Za-z0-9+.-]*:/.test(href)) {
      const baseHref = String(base && base.href !== undefined ? base.href : base);
      href = baseHref.replace(/[^/]*$/, '') + href;
    }
    this.href = href;
    const match = /^([A-Za-z][A-Za-z0-9+.-]*:)(?:\/\/([^/]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(href);
    this.protocol = match ? match[1] : '';
    this.host = match && match[2] ? match[2] : '';
    this.hostname = this.host.replace(/:\d+$/, '');
    this.port = this.host.slice(this.hostname.length).replace(/^:/, '');
    this.pathname = match ? match[3] : href;
    this.search = match && match[4] ? match[4] : '';
    this.hash = match && match[5] ? match[5] : '';
    this.origin = this.protocol === 'file:' ? 'null' : `${this.protocol}//${this.host}`;
    this.username = '';
    this.password = '';
  }
  toString() { return this.href; }
  toJSON() { return this.href; }
};

const URLSearchParamsCtor = globalThis.URLSearchParams || class URLSearchParams {
  constructor(init = '') { this.value = String(init).replace(/^\?/, ''); }
  toString() { return this.value; }
};

function invalidArgType(name, value) {
  const received = typeof value === 'string' ? `type string ('${value}')` :
    value === null ? 'null' : `type ${typeof value} (${String(value)})`;
  const expected = name === 'url' ? 'object' : 'string';
  const error = new TypeError(`The "${name}" argument must be of type ${expected}. Received ${received}`);
  error.code = 'ERR_INVALID_ARG_TYPE';
  return error;
}

if (typeof URLCtor.revokeObjectURL !== 'function') {
  URLCtor.revokeObjectURL = function revokeObjectURL(url) {
    if (arguments.length === 0) {
      const error = new TypeError('The "id" argument must be specified');
      error.code = 'ERR_MISSING_ARGS';
      throw error;
    }
    return undefined;
  };
}

function encodePath(path) {
  const safe = "!$&'()*+,-./0123456789:;=@ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz";
  const bytes = new TextEncoder().encode(path);
  let out = '';
  for (const byte of bytes) {
    const char = String.fromCharCode(byte);
    out += safe.includes(char) ? char : `%${byte.toString(16).toUpperCase().padStart(2, '0')}`;
  }
  return out;
}

function pathToFileURL(path, options) {
  if (typeof path !== 'string') throw invalidArgType('path', path);
  let absolute = path;
  const windows = options && options.windows !== undefined ? Boolean(options.windows) :
    (typeof process !== 'undefined' && process.platform === 'win32');
  if (windows && /^\\\\\?\\[A-Za-z]:/.test(absolute)) {
    absolute = absolute.slice(4);
  }
  if (windows && /^\\\\/.test(absolute)) {
    const parts = absolute.replace(/^\\\\\?\\UNC\\/i, '\\\\').slice(2).split('\\');
    const host = parts.shift();
    if (!host || !parts.length) {
      const error = new TypeError('Invalid UNC path');
      error.code = 'ERR_INVALID_ARG_VALUE';
      throw error;
    }
    if (/\s/.test(host)) {
      const error = new TypeError('Invalid URL');
      error.code = 'ERR_INVALID_URL';
      throw error;
    }
    return new URLCtor(`file://${host}/${encodePath(parts.join('/'))}`);
  }
  if (windows) {
    absolute = absolute.replace(/^\\\\\?\\/, '').replace(/\\/g, '/');
  }
  const isAbsolute = windows ? /^[A-Za-z]:\//.test(absolute) : absolute.startsWith('/');
  if (!isAbsolute) {
    let cwd = typeof process !== 'undefined' && process.cwd ? process.cwd() : '/';
    if (windows) cwd = cwd.replace(/\\/g, '/');
    absolute = cwd.replace(/\/$/, '') + '/' + absolute;
  }
  const href = (windows ? 'file:///' : 'file://') + encodePath(absolute);
  return new URLCtor(href);
}

function fileURLToPath(input, options) {
  let url;
  if (typeof input === 'string') url = new URLCtor(input);
  else if (input && typeof input.href === 'string') url = input;
  else throw invalidArgType('path', input);
  if (url.protocol !== 'file:') {
    const error = new TypeError('The URL must be of scheme file');
    error.code = 'ERR_INVALID_URL_SCHEME';
    throw error;
  }
  const windows = options && options.windows !== undefined ? Boolean(options.windows) :
    (typeof process !== 'undefined' && process.platform === 'win32');
  if (!windows && url.hostname && url.hostname !== 'localhost') {
    const error = new TypeError('File URL host must be empty or localhost');
    error.code = 'ERR_INVALID_FILE_URL_HOST';
    throw error;
  }
  if (/%2f/i.test(url.pathname) || (windows && /%5c/i.test(url.pathname))) {
    const error = new TypeError('File URL path must not include encoded / characters');
    error.code = 'ERR_INVALID_FILE_URL_PATH';
    error.input = url;
    throw error;
  }
  let path = decodeURIComponent(url.pathname);
  if (windows) {
    if (url.hostname) return `\\\\${url.hostname}${path.replace(/\//g, '\\')}`;
    if (/^\/[A-Za-z]:/.test(path)) path = path.slice(1).replace(/\//g, '\\');
  }
  return path;
}

function domainToASCII(domain) {
  if (typeof domain !== 'string') throw invalidArgType('domain', domain);
  try { return new URLCtor(`http://${domain}`).hostname; } catch { return ''; }
}

function domainToUnicode(domain) {
  if (typeof domain !== 'string') throw invalidArgType('domain', domain);
  return domain.split('.').map((label) => label.toLowerCase().startsWith('xn--') ?
    decodePunycode(label.slice(4)) : label).join('.');
}

function decodePunycode(input) {
  const output = [];
  const basic = input.lastIndexOf('-');
  let index = 0;
  if (basic >= 0) {
    for (; index < basic; index++) output.push(input.charCodeAt(index));
    index++;
  }
  let n = 128, i = 0, bias = 72;
  const adapt = (delta, points, first) => {
    delta = first ? Math.floor(delta / 700) : delta >> 1;
    delta += Math.floor(delta / points);
    let k = 0;
    while (delta > 455) { delta = Math.floor(delta / 35); k += 36; }
    return k + Math.floor((36 * delta) / (delta + 38));
  };
  while (index < input.length) {
    const old = i;
    let weight = 1;
    for (let k = 36; ; k += 36) {
      const code = input.charCodeAt(index++);
      const digit = code - 48 < 10 ? code - 22 : code - 97 < 26 ? code - 97 : code - 65;
      i += digit * weight;
      const threshold = k <= bias ? 1 : k >= bias + 26 ? 26 : k - bias;
      if (digit < threshold) break;
      weight *= 36 - threshold;
    }
    const length = output.length + 1;
    bias = adapt(i - old, length, old === 0);
    n += Math.floor(i / length);
    i %= length;
    output.splice(i++, 0, n);
  }
  return String.fromCodePoint(...output);
}

function urlToHttpOptions(url) {
  if (url === null || typeof url !== 'object') throw invalidArgType('url', url);
  const value = url;
  const options = {
    protocol: value.protocol,
    hostname: typeof value.hostname === 'string' ? value.hostname.replace(/^\[|\]$/g, '') : value.hostname,
    hash: value.hash,
    search: value.search,
    pathname: value.pathname,
    path: `${value.pathname || ''}${value.search || ''}`,
    href: value.href,
  };
  if (value.port !== '') options.port = Number(value.port);
  if (typeof value.href === 'string') {
    const authorityStart = value.href.indexOf('//');
    const authorityEnd = value.href.indexOf('/', authorityStart + 2);
    const authority = value.href.slice(authorityStart + 2, authorityEnd < 0 ? undefined : authorityEnd);
    const at = authority.lastIndexOf('@');
    if (at >= 0) options.auth = decodeURIComponent(authority.slice(0, at));
  }
  return options;
}

function format(value, options) {
  if (typeof value === 'string') {
    return legacyParse(value).format();
  }
  if (!value || typeof value !== 'object') {
    throw invalidArgType('urlObject', value);
  }
  if (value instanceof URLCtor || (value && typeof value.href === 'string')) {
    if (options !== undefined && (options === null || typeof options !== 'object')) {
      throw invalidArgType('options', options);
    }
    const opts = options || {};
    let href = value.href;
    if (opts.auth !== undefined && !opts.auth) {
      const scheme = href.indexOf('//');
      const at = href.indexOf('@', scheme + 2);
      if (scheme >= 0 && at >= 0) href = href.slice(0, scheme + 2) + href.slice(at + 1);
    }
    if (opts.search !== undefined && !opts.search) href = href.replace(/\?[^#]*/, '');
    if (opts.fragment !== undefined && !opts.fragment) href = href.replace(/#.*/, '');
    if (opts.unicode) {
      const scheme = href.indexOf('//');
      const end = href.indexOf('/', scheme + 2);
      const authority = href.slice(scheme + 2, end < 0 ? undefined : end);
      const at = authority.lastIndexOf('@');
      const prefix = at >= 0 ? authority.slice(0, at + 1) : '';
      const hostPort = at >= 0 ? authority.slice(at + 1) : authority;
      const colon = hostPort.lastIndexOf(':');
      const host = colon > 0 ? hostPort.slice(0, colon) : hostPort;
      const port = colon > 0 ? hostPort.slice(colon) : '';
      href = href.slice(0, scheme + 2) + prefix + domainToUnicode(host) + port +
        (end < 0 ? '' : href.slice(end));
    }
    return href;
  }
  return Url.prototype.format.call(value);
}

module.exports = {
  URL: URLCtor,
  URLSearchParams: URLSearchParamsCtor,
  Url,
  parse: legacyParse,
  format,
  resolve: legacyResolve,
  resolveObject: legacyResolveObject,
  domainToASCII,
  domainToUnicode,
  pathToFileURL,
  fileURLToPath,
  urlToHttpOptions,
};
