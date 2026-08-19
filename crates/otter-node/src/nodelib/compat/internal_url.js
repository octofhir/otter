'use strict';
// internal/url — URL plus the http-facing helpers the vendored client uses.

function isURL(value) {
  return typeof globalThis.URL === 'function' && value instanceof globalThis.URL;
}

function pathToFileURL(path) {
  return new globalThis.URL(`file://${String(path)}`);
}

// §url.fileURLToPath — the path a `file:` URL names, with percent escapes
// resolved. Anything else is already a path.
function fileURLToPath(url) {
  const parsed = typeof url === 'string' ? new globalThis.URL(url) : url;
  if (parsed.protocol !== 'file:') {
    const error = new TypeError('The URL must be of scheme file');
    error.code = 'ERR_INVALID_URL_SCHEME';
    throw error;
  }
  return decodeURIComponent(parsed.pathname);
}

/// A `file:` URL becomes the path it names; everything else passes through.
function toPathIfFileURL(fileURLOrPath) {
  if (!isURL(fileURLOrPath)) return fileURLOrPath;
  return fileURLToPath(fileURLOrPath);
}

// §https://nodejs.org/api/url.html#urlurltohttpoptionsurl
function urlToHttpOptions(url) {
  const { hostname, pathname, port, username, password, search } = url;
  const options = {
    __proto__: null,
    ...url, // In case the url object was extended by the user.
    protocol: url.protocol,
    hostname: typeof hostname === 'string' && hostname.startsWith('[') ?
      hostname.slice(1, -1) :
      hostname,
    hash: url.hash,
    search,
    pathname,
    path: `${pathname || ''}${search || ''}`,
    href: url.href,
  };
  if (port !== '') {
    options.port = Number(port);
  }
  if (username || password) {
    options.auth = `${decodeURIComponent(username)}:${decodeURIComponent(password)}`;
  }
  return options;
}

// The URL classes are web globals; a runtime built without the web APIs
// still loads this module, so they are read when asked for rather than at
// load time.
module.exports = {
  get URL() { return globalThis.URL; },
  get URLSearchParams() { return globalThis.URLSearchParams; },
  isURL,
  pathToFileURL,
  fileURLToPath,
  toPathIfFileURL,
  urlToHttpOptions,
  domainToASCII: (domain) => String(domain).toLowerCase(),
  domainToUnicode: (domain) => String(domain),
};
